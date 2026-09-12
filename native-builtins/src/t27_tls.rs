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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

#[cfg(unix)]
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
#[cfg(unix)]
use openssl::{pkey::PKey, x509::X509};
use parking_lot::Mutex;
use rustls::client::{ClientConnection, ResolvesClientCert};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert, ServerConnection, WebPkiClientVerifier};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore, ServerConfig, SignatureScheme, StreamOwned};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::RuntimeError;
use cratonvm_types::{ObjectRef, Value};

use cratonvm_native_io::eintr::EintrIo;

use crate::servlet;
use crate::try_alloc_concurrent_synthetic;
use cratonvm_types::error::MethodCallFailed;

thread_local! {
    // Re-entrancy guard for the interface-level `HostnameVerifier.verify`
    // native. When we re-dispatch a custom verifier's `verify()` via
    // `ctx.invoke`, the VM's interface resolution could (in the degenerate
    // case of a subclass that defines no bytecode override) route back into
    // this same native. The guard breaks that loop: a re-entrant call falls
    // back to the default "accept — rustls already validated SNI" behavior
    // instead of recursing forever.
    static HOSTNAME_VERIFY_REENTRANT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

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
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
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
pub(crate) fn set_runtime_tls_identity(identity: Option<RuntimeTlsIdentity>) {
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
// Per-SSLContext TLS identity (mTLS / in-process client+server)
// -----------------------------------------------------------------------------
//
// The process-global `RuntimeTlsIdentity` above cannot represent an in-process
// test that stands up BOTH a TLS server and a TLS client (e.g.
// `TestClientCertTls13`): the server keystore (localhost cert) and the client
// keystore (client cert) both flow through the keystore load path and clobber
// the single global slot. We therefore also track identity PER `SSLContext`.
//
// Flow (matches how JSSE wires up, same thread): `KeyManagerFactory.init(ks,
// pass)` stashes that keystore's (cert_pem, key_pem) in a thread-local; the
// following `SSLContext.init(keyManagers, …)` moves it into a table keyed by the
// SSLContext's object identity. `createSSLEngine()` copies it onto the engine
// (server cert); `getSocketFactory().createSocket()` uses it as the rustls
// client-auth identity (client-cert presentation). Keying off the thread-local
// at KMF.init — rather than reading the `KeyManager` objects at SSLContext.init
// — sidesteps test wrappers like `TrackingKeyManager`.

thread_local! {
    static PENDING_KM_IDENTITY: std::cell::RefCell<Option<(String, String)>> =
        const { std::cell::RefCell::new(None) };
    static PENDING_TM_TRUST_ROOTS: std::cell::RefCell<Option<TlsTrustRoots>> =
        const { std::cell::RefCell::new(None) };
    static SELECTED_CONTEXT_TRUST_ROOTS: std::cell::RefCell<Option<TlsTrustRoots>> =
        const { std::cell::RefCell::new(None) };
}

/// `KeyManagerFactory.init` calls this with the keystore's PEM identity.
pub fn set_pending_km_identity(cert_pem: String, key_pem: String) {
    PENDING_KM_IDENTITY.with(|c| *c.borrow_mut() = Some((cert_pem, key_pem)));
}

fn take_pending_km_identity() -> Option<(String, String)> {
    PENDING_KM_IDENTITY.with(|c| c.borrow_mut().take())
}

#[derive(Clone, Debug, Default)]
struct TlsTrustRoots {
    root_ders: Vec<Vec<u8>>,
    /// OCSP revocation-checking configuration for this trust manager, if any
    /// was attached via `PKIXBuilderParameters.addCertPathChecker(...)`. Set
    /// separately from `root_ders` (via `set_pending_tm_revocation`, called
    /// right alongside `set_pending_tm_trust_roots` from
    /// `x509_manager::tmf_engine_init_params` — the only producer that ever
    /// has a non-`None` `RevocationConfig`) and carried through the same
    /// thread-local/HUC-identity-capture lifecycle so the native
    /// `HttpURLConnection` client path (`http_url_connection.rs`), which
    /// never calls back into Java `TrustManager.checkServerTrusted` and so
    /// never reaches `x509_manager::validate_chain` on its own, still gets a
    /// chance to perform real OCSP checking (see
    /// `OcspAwareServerCertVerifier`).
    revocation: Option<crate::x509_manager::RevocationConfig>,
}

/// `TrustManagerFactory.init` calls this with the exact anchor set that should
/// be scoped to the next `SSLContext.init` on this thread. A configured custom
/// truststore is therefore restrictive instead of being added to native roots.
pub(crate) fn set_pending_tm_trust_roots(root_ders: Vec<Vec<u8>>) {
    let mut deduped: Vec<Vec<u8>> = Vec::new();
    for der in root_ders {
        if !der.is_empty() && !deduped.iter().any(|r| r == &der) {
            deduped.push(der);
        }
    }
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] set_pending_tm_trust_roots count={}",
            deduped.len()
        );
    }
    PENDING_TM_TRUST_ROOTS.with(|c| {
        let mut slot = c.borrow_mut();
        // Preserve a revocation config set via `set_pending_tm_revocation`
        // for the same pending trust-manager init, whichever order the two
        // calls happen in (today `set_pending_tm_revocation` is always
        // called first by `tmf_engine_init_params`, but this is defensive).
        let revocation = slot.take().and_then(|prev| prev.revocation);
        *slot = Some(TlsTrustRoots {
            root_ders: deduped,
            revocation,
        });
    });
}

/// `tmf_engine_init_params` calls this (right before/after
/// `set_pending_tm_trust_roots`) with the `RevocationConfig` extracted from
/// any `PKIXRevocationChecker` attached to the `PKIXBuilderParameters`. Every
/// other `TrustManagerState` producer never has a `RevocationConfig` to
/// contribute, so this is a separate setter rather than a new parameter on
/// `set_pending_tm_trust_roots` (which has several call sites that would
/// otherwise need an unused `None` threaded through them).
pub(crate) fn set_pending_tm_revocation(revocation: Option<crate::x509_manager::RevocationConfig>) {
    PENDING_TM_TRUST_ROOTS.with(|c| {
        let mut slot = c.borrow_mut();
        match slot.as_mut() {
            Some(existing) => existing.revocation = revocation,
            None => {
                *slot = Some(TlsTrustRoots {
                    root_ders: Vec::new(),
                    revocation,
                });
            }
        }
    });
}

fn take_pending_tm_trust_roots() -> Option<TlsTrustRoots> {
    let out = PENDING_TM_TRUST_ROOTS.with(|c| c.borrow_mut().take());
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] take_pending_tm_trust_roots -> {:?}",
            out.as_ref().map(|r| r.root_ders.len())
        );
    }
    out
}

fn set_selected_context_trust_roots(roots: Option<TlsTrustRoots>) {
    SELECTED_CONTEXT_TRUST_ROOTS.with(|c| *c.borrow_mut() = roots);
}

fn selected_context_trust_roots() -> Option<TlsTrustRoots> {
    SELECTED_CONTEXT_TRUST_ROOTS.with(|c| c.borrow().clone())
}

#[cfg(unix)]
pub(crate) fn selected_context_trust_root_ders() -> Vec<Vec<u8>> {
    selected_context_trust_roots()
        .map(|roots| roots.root_ders)
        .unwrap_or_default()
}

/// Return anchors attached to a specific SSLContext.  Unlike the selected
/// context slot, this remains available after an SSL factory crosses into a
/// different Java thread.
pub(crate) fn context_trust_root_ders(
    ctx: &mut dyn NativeContext,
    context: ObjectRef,
) -> Result<Vec<Vec<u8>>, MethodCallFailed> {
    let key = ctx_obj_key(ctx, context);
    Ok(ctx_trust_roots_table()
        .lock()
        .get(&key?)
        .map(|roots| roots.root_ders.clone())
        .unwrap_or_default())
}

#[cfg(unix)]
pub(crate) fn is_dsa_private_key_pem(key_pem: &str) -> bool {
    openssl::pkey::PKey::private_key_from_pem(key_pem.as_bytes()).is_ok_and(|key| key.dsa().is_ok())
}

#[cfg(unix)]
pub(crate) fn is_dsa_certificate_der(der: &[u8]) -> bool {
    openssl::x509::X509::from_der(der)
        .ok()
        .and_then(|cert| cert.public_key().ok())
        .is_some_and(|key| key.dsa().is_ok())
}

fn take_selected_context_trust_roots() -> Option<TlsTrustRoots> {
    SELECTED_CONTEXT_TRUST_ROOTS.with(|c| c.borrow_mut().take())
}

fn ctx_identity_table() -> &'static Mutex<HashMap<u64, (String, String)>> {
    static T: OnceLock<Mutex<HashMap<u64, (String, String)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ctx_trust_roots_table() -> &'static Mutex<HashMap<u64, TlsTrustRoots>> {
    static T: OnceLock<Mutex<HashMap<u64, TlsTrustRoots>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The `TrustManager[]` objects passed to `SSLContext.init(km, tms, random)`,
/// keyed the same way as `ctx_trust_roots_table`. Populated by
/// `attach_trust_managers_to_ctx`, consulted post-handshake via
/// `ctx_trust_managers`/`engine_run_trust_check`. Holds live `ObjectRef`s —
/// unlike its sibling tables here (which only ever held derived PEM/DER
/// bytes) — so it MUST stay in the GC root set: see
/// `gc_scan_tls_ctx_trust_manager_roots`/`gc_update_tls_ctx_trust_manager_refs`
/// (wired into `vm/src/memory/roots.rs` and `gc.rs`).
fn ctx_trust_managers_table() -> &'static Mutex<HashMap<u64, Vec<ObjectRef>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<ObjectRef>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ctx_obj_key(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Result<u64, MethodCallFailed> {
    Ok(crate::gc_stable_lock_key(ctx, obj)? as u64)
}

/// The TLS session stores belonging to an `SSLContext`, keyed the same way as
/// its TrustManagers.
///
/// **This is what makes resumption possible at all.** A rustls `ClientConfig`
/// owns the client-side session store and a `ServerConfig` owns the
/// server-side one, and `engine_begin` builds a FRESH config for every engine
/// (it has to — ciphers, protocols, ALPN and client-auth mode are per-engine
/// settings). Every ticket was therefore thrown away with the engine that
/// received it, and two engines of one `SSLContext` could never resume.
/// `HUC_DEFAULT_CLIENT_CONFIG` caches a whole config for the same reason on
/// the `HttpsURLConnection` path, where there are no per-engine settings to
/// lose.
///
/// Only the STORES are shared, never the configs: a shared config would make
/// one engine's `setEnabledCipherSuites` silently govern the next engine's
/// handshake.
///
/// Plain `Arc`s, no heap `ObjectRef`s — nothing for the GC to scan.
#[allow(clippy::type_complexity)]
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — one acquisition site
/// (`ctx_client_session_store`), an `entry(key).or_insert_with(..)` whose
/// closure builds a rustls store and touches no `ctx`.
fn ctx_client_session_store_table() -> &'static cratonvm_types::lock_order::OrderedPlMutex<
    HashMap<u64, Arc<dyn rustls::client::ClientSessionStore>>,
> {
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<
            HashMap<u64, Arc<dyn rustls::client::ClientSessionStore>>,
        >,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Built `ClientConfig`s, keyed by `(SSLContext key, engine shape)`.
///
/// **Why the whole config and not just the session store.** rustls refuses to
/// resume a TLS 1.3 session unless the `ServerCertVerifier` AND the
/// `ResolvesClientCert` are the *same `Arc`* as when the session was stored
/// (`persist::Tls13ClientSessionValue::compatible_config`, pointer identity —
/// a deliberate rule: resuming across a different verifier would silently
/// inherit a trust decision the new verifier never made). `engine_begin`
/// builds a fresh config, and therefore a fresh verifier, per engine, so the
/// ticket was always discarded and every handshake was `Full` — the client
/// took the ticket out of the store and then never offered it.
///
/// The key is the whole engine shape because those settings genuinely change
/// the config: sharing across different cipher/protocol restrictions would let
/// one engine's `setEnabledCipherSuites` govern the next engine's handshake.
///
/// Plain `Arc`s, no heap `ObjectRef`s — nothing for the GC to scan.
#[allow(clippy::type_complexity)]
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — two acquisition sites, both
/// temporary guards over an already-built key: a `.get(..).cloned()` inside an
/// `and_then` closure and an `entry(..).or_insert(config).clone()`.
fn ctx_client_config_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<(u64, String), Arc<ClientConfig>>>
{
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<HashMap<(u64, String), Arc<ClientConfig>>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

fn ctx_client_session_store(key: u64) -> Arc<dyn rustls::client::ClientSessionStore> {
    ctx_client_session_store_table()
        .lock()
        .entry(key)
        .or_insert_with(|| {
            let inner: Arc<dyn rustls::client::ClientSessionStore> =
                Arc::new(rustls::client::ClientSessionMemoryCache::new(256));
            if crate::nbflags().dbg_tls_auth_ok {
                Arc::new(TracingClientSessionStore { inner })
            } else {
                inner
            }
        })
        .clone()
}

/// `CRATONVM_DBG_TLS_AUTH=1` only: says whether a ticket was stored and
/// whether the next connection took one, which is the difference between "the
/// server never issued one" and "the client never offered it".
#[derive(Debug)]
struct TracingClientSessionStore {
    inner: Arc<dyn rustls::client::ClientSessionStore>,
}

impl rustls::client::ClientSessionStore for TracingClientSessionStore {
    fn set_kx_hint(
        &self,
        server_name: rustls::pki_types::ServerName<'static>,
        group: rustls::NamedGroup,
    ) {
        self.inner.set_kx_hint(server_name, group)
    }
    fn kx_hint(
        &self,
        server_name: &rustls::pki_types::ServerName<'_>,
    ) -> Option<rustls::NamedGroup> {
        self.inner.kx_hint(server_name)
    }
    fn set_tls12_session(
        &self,
        server_name: rustls::pki_types::ServerName<'static>,
        value: rustls::client::Tls12ClientSessionValue,
    ) {
        eprintln!("[dbg-tls-auth] client store: set_tls12_session {server_name:?}");
        self.inner.set_tls12_session(server_name, value)
    }
    fn tls12_session(
        &self,
        server_name: &rustls::pki_types::ServerName<'_>,
    ) -> Option<rustls::client::Tls12ClientSessionValue> {
        let v = self.inner.tls12_session(server_name);
        eprintln!(
            "[dbg-tls-auth] client store: tls12_session {server_name:?} -> {}",
            v.is_some()
        );
        v
    }
    fn remove_tls12_session(&self, server_name: &rustls::pki_types::ServerName<'static>) {
        self.inner.remove_tls12_session(server_name)
    }
    fn insert_tls13_ticket(
        &self,
        server_name: rustls::pki_types::ServerName<'static>,
        value: rustls::client::Tls13ClientSessionValue,
    ) {
        eprintln!("[dbg-tls-auth] client store: insert_tls13_ticket {server_name:?}");
        self.inner.insert_tls13_ticket(server_name, value)
    }
    fn take_tls13_ticket(
        &self,
        server_name: &rustls::pki_types::ServerName<'static>,
    ) -> Option<rustls::client::Tls13ClientSessionValue> {
        let v = self.inner.take_tls13_ticket(server_name);
        eprintln!(
            "[dbg-tls-auth] client store: take_tls13_ticket {server_name:?} -> {}",
            v.is_some()
        );
        v
    }
}

#[allow(clippy::type_complexity)]
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — the server twin of
/// `ctx_client_session_store_table`, same single `or_insert_with` site.
fn ctx_server_session_store_table() -> &'static cratonvm_types::lock_order::OrderedPlMutex<
    HashMap<(u64, bool), Arc<dyn rustls::server::StoresServerSessions + Send + Sync>>,
> {
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<
            HashMap<(u64, bool), Arc<dyn rustls::server::StoresServerSessions + Send + Sync>>,
        >,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// One session cache per `SSLContext` **and per client-auth policy**.
///
/// The `bool` half of that key is load-bearing, and the bug it fixes is
/// `TestClientCert`'s entire failing set. A resumed TLS 1.2 session replays the
/// original handshake's outcome: the server sends no `CertificateRequest`, so
/// the client is never asked for a certificate. A connection that IS requesting
/// client auth must therefore not be allowed to resume a session established
/// WITHOUT it — the resumption silently cancels the request.
///
/// That is precisely what defeated `wants_deferred_client_auth` (see its doc).
/// The deferred mechanism exists to answer Tomcat's post-handshake
/// `setNeedClientAuth(true)` by offering client auth on the NEXT connection,
/// because rustls has no renegotiation. MEASURED on `dev@151f7831a`: it did
/// offer it — `engine_begin request=true` on the second engine — and then the
/// client resumed the first connection's no-client-auth session, so
/// `JavaKeyManagerResolver::resolve` was called **zero** times across the whole
/// class against **16** `has_certs` calls. The resolver was fully configured
/// and simply never consulted.
///
/// Sessions established WITH client auth still resume among themselves, so the
/// cost is one full handshake per policy transition, not per connection.
///
/// Real JSSE reaches the same outcome by a different route: its session object
/// carries the peer certificates, and it declines to resume into a connection
/// whose client-auth requirement that session cannot satisfy. rustls's
/// `StoresServerSessions` is an opaque blob store with no such visibility,
/// which is why the partition lives in the KEY rather than in a predicate.
fn ctx_server_session_store(
    key: u64,
    client_auth_requested: bool,
) -> Arc<dyn rustls::server::StoresServerSessions + Send + Sync> {
    ctx_server_session_store_table()
        .lock()
        .entry((key, client_auth_requested))
        .or_insert_with(|| {
            let inner: Arc<dyn rustls::server::StoresServerSessions + Send + Sync> =
                rustls::server::ServerSessionMemoryCache::new(256);
            if crate::nbflags().dbg_tls_auth_ok {
                Arc::new(TracingServerSessionStore { inner })
            } else {
                inner
            }
        })
        .clone()
}

/// `CRATONVM_DBG_TLS_AUTH=1` only — the server half of
/// [`TracingClientSessionStore`].
#[derive(Debug)]
struct TracingServerSessionStore {
    inner: Arc<dyn rustls::server::StoresServerSessions + Send + Sync>,
}

impl rustls::server::StoresServerSessions for TracingServerSessionStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool {
        let ok = self.inner.put(key.clone(), value);
        eprintln!(
            "[dbg-tls-auth] server store: put len={} -> {}",
            key.len(),
            ok
        );
        ok
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let v = self.inner.get(key);
        eprintln!(
            "[dbg-tls-auth] server store: get len={} -> {}",
            key.len(),
            v.is_some()
        );
        v
    }
    fn take(&self, key: &[u8]) -> Option<Vec<u8>> {
        let v = self.inner.take(key);
        eprintln!(
            "[dbg-tls-auth] server store: take len={} -> {}",
            key.len(),
            v.is_some()
        );
        v
    }
    fn can_cache(&self) -> bool {
        self.inner.can_cache()
    }
}

/// `SSLContext.init(km, tms, random)` calls this with the raw `tms` array
/// argument (may be `None`/empty) to stash the actual `TrustManager` Java
/// objects against this context, keyed the same way as the KMF identity /
/// TMF trust-root PEM tables. See the `EngineState::trust_managers_ctx_key`
/// doc for why the engine only ever stores the KEY, never these `ObjectRef`s
/// directly.
pub(crate) fn attach_trust_managers_to_ctx(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
    tms_array: Option<ObjectRef>,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let mut list = Vec::new();
    if let Some(arr) = tms_array {
        let len = ctx.array_length(arr);
        for i in 0..len {
            if let Value::Object(Some(tm)) = ctx.get_array_element(arr, i) {
                list.push(tm);
            }
        }
    }
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] attach_trust_managers_to_ctx key={} tms_array_present={} count={}",
            key,
            tms_array.is_some(),
            list.len()
        );
    }
    // FIX (tls-handshake-enforcement-gap, doc 21): snapshot each manager's
    // accepted issuers now, while we still hold a `ctx` to call Java with.
    //
    // A server that delegates trust to an application `TrustManager` has no
    // keystore-derived CA (Tomcat's `trustManagerClassName` mechanism), so
    // `PassthroughClientCertVerifier` is used — and its `root_hint_subjects`
    // was hard-coded empty, meaning the `CertificateRequest` carried no
    // acceptable-CA list at all. Real JSSE sends the manager's
    // `getAcceptedIssuers()` there, and clients rely on it:
    // `TestCustomSslTrustManager.testCustomTrustManagerCA` asserts that its
    // `KeyManager.chooseClientAlias` was offered EXACTLY the test CA.
    //
    // Captured here (rather than when the config is built) because
    // `engine_begin` runs with no native context. A failure to call the
    // manager is not fatal — the hints are an optimisation for the peer, and
    // an empty list is exactly the old behaviour — so errors are swallowed
    // rather than propagated out of `SSLContext.init`.
    // GC (FIXED 2026-08-01): `list` holds raw `ObjectRef`s that are NOT yet in
    // `ctx_trust_managers_table`, so `gc_scan_tls_ctx_trust_manager_roots`
    // cannot see them and `gc_update_tls_ctx_trust_manager_refs` cannot remap
    // them. `capture_accepted_issuer_dns` runs three `invoke_virtual`s per
    // manager and walks two arrays — it allocates freely. A moving young
    // collection landing in there relocated every manager and left `list`
    // naming the vacated slots, which then went into the table VERBATIM and
    // stayed wrong for the whole life of that `SSLContext`. The next
    // `checkServerTrusted` on it resolved the receiver's class as
    // `java.lang.Object` — ClassId(0), the reclaimed-slot signature — and the
    // handshake failed as `SSLHandshakeException: TrustManager rejected the
    // peer certificate chain`, ~1 run in 20 of `TestSSLHostConfigCompat`.
    //
    // `CRATONVM_GC=-moving-young` is what localised it: 14/14 clean with the
    // non-moving sweep, which relocates nothing. That also proves the entry is
    // correctly ROOTED once it reaches the table (a sweep would have freed an
    // unrooted manager just the same) — the gap was only ever this window
    // before the insert.
    let pins: Vec<usize> = list.iter().map(|tm| ctx.pin_native_root(*tm)).collect();
    let issuers = capture_accepted_issuer_dns(ctx, &list, &pins);
    // Re-read every manager through its pin before the table takes ownership.
    for (i, tm) in list.iter_mut().enumerate() {
        *tm = ctx.read_native_pin(pins[i], *tm);
    }
    if let Some(&base) = pins.first() {
        ctx.unpin_native_roots(base);
    }
    // Whether endpoint identification is THIS VM's job for engines of this
    // context, decided here for the same reason the issuer hints are:
    // `engine_begin` — which builds the `ClientConfig` that carries the
    // in-handshake check — runs with no native context, and this question
    // needs one (it walks the manager's class hierarchy).
    let identifies = jsse_owns_endpoint_identification(ctx, &list);
    let mut table = ctx_trust_managers_table().lock();
    if list.is_empty() {
        table.remove(&key);
        ctx_accepted_issuers_table().lock().remove(&key);
        ctx_jsse_identifies_table().lock().remove(&key);
    } else {
        table.insert(key, list);
        ctx_accepted_issuers_table().lock().insert(key, issuers);
        ctx_jsse_identifies_table().lock().insert(key, identifies);
    }
    Ok(())
}

/// Does endpoint identification fall to THIS VM for engines created by this
/// `SSLContext`? See [`jsse_owns_endpoint_identification`] for the rule and
/// `attach_trust_managers_to_ctx` for why it is answered at attach time.
///
/// Absent = `true`: no application manager is installed, so JSSE's own default
/// (which this VM stands in for) is the one that identifies.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — three sites, all one-statement
/// `remove` / `insert` / `.get(&key).copied()` over a key built beforehand.
fn ctx_jsse_identifies_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, bool>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, bool>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

fn ctx_jsse_identifies(ctx_key: Option<u64>) -> bool {
    let Some(key) = ctx_key else {
        return true;
    };
    ctx_jsse_identifies_table()
        .lock()
        .get(&key)
        .copied()
        .unwrap_or(true)
}

/// DER-encoded subject DNs of every `TrustManager`'s accepted issuers, keyed
/// like `ctx_trust_managers_table`. See `attach_trust_managers_to_ctx`.
fn ctx_accepted_issuers_table() -> &'static Mutex<HashMap<u64, Vec<Vec<u8>>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<Vec<u8>>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Accepted-issuer DNs recorded for `ctx_key`, as rustls
/// `DistinguishedName`s ready to go into a `CertificateRequest`.
fn accepted_issuer_hints(ctx_key: Option<u64>) -> Vec<rustls::DistinguishedName> {
    let Some(key) = ctx_key else {
        return Vec::new();
    };
    ctx_accepted_issuers_table()
        .lock()
        .get(&key)
        .map(|ders| {
            ders.iter()
                .map(|d| rustls::DistinguishedName::from(d.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Call `X509TrustManager.getAcceptedIssuers()` on each manager and collect
/// the DER encoding of every returned certificate's subject DN.
///
/// Capped, because a manager backed by the platform trust store legitimately
/// returns ~150 roots and putting all of them in every `CertificateRequest`
/// would bloat each handshake for no benefit to the callers this exists for.
///
/// `pins` holds one native-root pin per entry of `managers`, taken by the
/// caller BEFORE this runs. Every `invoke_virtual` below allocates, so each
/// manager is re-read through its pin at the top of the loop rather than
/// dereferenced from the caller's raw copy — see `attach_trust_managers_to_ctx`
/// for the defect that motivated it.
fn capture_accepted_issuer_dns(
    ctx: &mut dyn NativeContext,
    managers: &[ObjectRef],
    pins: &[usize],
) -> Vec<Vec<u8>> {
    const MAX_ISSUER_HINTS: usize = 16;
    let mut out: Vec<Vec<u8>> = Vec::new();
    for (mi, tm) in managers.iter().enumerate() {
        if out.len() >= MAX_ISSUER_HINTS {
            break;
        }
        let tm = match pins.get(mi) {
            Some(&p) => ctx.read_native_pin(p, *tm),
            None => *tm,
        };
        let certs0 = match ctx.invoke_virtual(
            tm,
            "getAcceptedIssuers",
            "()[Ljava/security/cert/X509Certificate;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(arr)))) => arr,
            _ => continue,
        };
        // `certs` outlives the allocating calls in the body below, so it is
        // pinned too and re-read on every iteration.
        let certs_pin = ctx.pin_native_root(certs0);
        let certs = ctx.read_native_pin(certs_pin, certs0);
        let len = ctx.array_length(certs);
        for i in 0..len {
            if out.len() >= MAX_ISSUER_HINTS {
                break;
            }
            let certs = ctx.read_native_pin(certs_pin, certs0);
            let Value::Object(Some(cert)) = ctx.get_array_element(certs, i) else {
                continue;
            };
            let principal = match ctx.invoke_virtual(
                cert,
                "getSubjectX500Principal",
                "()Ljavax/security/auth/x500/X500Principal;",
                &[],
            ) {
                Ok(Some(Value::Object(Some(p)))) => p,
                _ => continue,
            };
            let encoded = match ctx.invoke_virtual(principal, "getEncoded", "()[B", &[]) {
                Ok(Some(Value::Object(Some(b)))) => b,
                _ => continue,
            };
            let n = ctx.array_length(encoded);
            let mut der = vec![0u8; n];
            let read = ctx.read_byte_array_into(encoded, 0, &mut der);
            if read == n && n > 0 {
                out.push(der);
            }
        }
        ctx.unpin_native_roots(certs_pin);
    }
    out
}

/// The `ctx_trust_managers_table` key for `ctx_obj`, if real Java
/// `TrustManager` objects were attached at `SSLContext.init` time
/// (`attach_trust_managers_to_ctx`) — `None` when the context carries no
/// custom trust managers. Used by the native client-socket paths
/// (`phases_late::new13_do_create_socket`) to decide whether certificate
/// verification must be delegated to the Java TrustManagers.
pub(crate) fn ctx_trust_managers_key_if_attached(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
) -> Result<Option<u64>, MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    if ctx_trust_managers_table().lock().contains_key(&key) {
        Ok(Some(key))
    } else {
        Ok(None)
    }
}

/// The `KeyManager[]` objects passed to `SSLContext.init(km, tms, random)`,
/// keyed identically to `ctx_trust_managers_table` (same reasons apply: holds
/// live `ObjectRef`s, so it MUST stay in the GC root set — see
/// `gc_scan_tls_ctx_key_manager_roots`/`gc_update_tls_ctx_key_manager_refs`,
/// wired into `vm/src/memory/roots.rs` and `gc.rs`). Consulted synchronously
/// mid-handshake by `JavaKeyManagerResolver::resolve` (via a `km_ctx_key`
/// looked up here, never the `ObjectRef`s copied out long-term — the same
/// "keep only a key" discipline `EngineState::trust_managers_ctx_key` uses)
/// so a real `KeyManager.chooseClientAlias` can be consulted for mTLS client
/// certificate selection instead of presenting one fixed identity.
fn ctx_key_managers_table() -> &'static Mutex<HashMap<u64, Vec<ObjectRef>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<ObjectRef>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `SSLContext.init(km, tms, random)` calls this with the raw `km` array
/// argument (may be `None`/empty) to stash the actual `KeyManager` Java
/// objects against this context, keyed the same way as
/// `attach_trust_managers_to_ctx`.
pub(crate) fn attach_key_managers_to_ctx(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
    kms_array: Option<ObjectRef>,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let mut list = Vec::new();
    if let Some(arr) = kms_array {
        let len = ctx.array_length(arr);
        for i in 0..len {
            if let Value::Object(Some(km)) = ctx.get_array_element(arr, i) {
                list.push(km);
            }
        }
    }
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] attach_key_managers_to_ctx key={} kms_array_present={} count={}",
            key,
            kms_array.is_some(),
            list.len()
        );
    }
    let mut table = ctx_key_managers_table().lock();
    if list.is_empty() {
        table.remove(&key);
    } else {
        table.insert(key, list);
    }
    Ok(())
}

/// `SSLContext.init` calls this to move pending KMF identity and TMF trust
/// roots onto the SSLContext object's per-context slots.
///
/// `resolved_km_identity` is the identity resolved DIRECTLY from the actual
/// `KeyManager[]` this `SSLContext.init` call received (via
/// `x509_manager::resolved_identity_pem_for_key_manager_array`), when the
/// caller could trace it.
/// It takes priority over the thread-local `PENDING_KM_IDENTITY`, which is
/// only "whichever `KeyManagerFactory.init` ran most recently on this
/// thread" -- correct when a KMF.init is immediately followed by its own
/// SSLContext.init, but wrong when Netty/Reactor Netty builds more than one
/// SSLContext from more than one KeyManagerFactory before either is actually
/// used (e.g. a protocol-support probe context built ahead of the real one):
/// an intervening, unrelated SSLContext.init can drain the thread-local
/// before the context that actually needs it ever consumes it, silently
/// leaving that context — and every engine created from it — with no
/// identity at all (see pemcertificates-clientauth-rustls-decrypterror).
/// The thread-local is drained unconditionally either way so it never leaks
/// into a later, unrelated SSLContext.init.
pub(crate) fn attach_pending_identity_to_ctx(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
    resolved_km_identity: Option<(String, String)>,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let pending = take_pending_km_identity();
    if let Some(ident) = resolved_km_identity.or(pending) {
        if crate::nbflags().dbg_tls_auth {
            eprintln!(
                "[dbg-tls-auth] attach_pending_identity_to_ctx key={} STORING km identity key_pem_len={} cert_pem_len={}",
                key,
                ident.1.len(),
                ident.0.len()
            );
        }
        ctx_identity_table().lock().insert(key, ident);
    } else if crate::nbflags().dbg_tls_auth {
        eprintln!(
            "[dbg-tls-auth] attach_pending_identity_to_ctx key={} NO pending km identity to store",
            key
        );
    }
    if let Some(roots) = take_pending_tm_trust_roots() {
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] attach_pending_identity_to_ctx key={} storing {} roots",
                key,
                roots.root_ders.len()
            );
        }
        ctx_trust_roots_table().lock().insert(key, roots);
    } else if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] attach_pending_identity_to_ctx key={} NO pending roots to store",
            key
        );
    }
    Ok(())
}

/// Look up the identity previously associated with an `SSLContext` object.
pub(crate) fn ctx_identity(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
) -> Result<Option<(String, String)>, MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let trust_roots = ctx_trust_roots_table().lock().get(&key).cloned();
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] ctx_identity key={} trust_roots={:?}",
            key,
            trust_roots.as_ref().map(|r| r.root_ders.len())
        );
    }
    set_selected_context_trust_roots(trust_roots);
    Ok(ctx_identity_table().lock().get(&key).cloned())
}

/// Convert a private-key DER (PKCS#8, PKCS#1, or SEC1) + DER cert chain (leaf
/// first) to the (cert_pem, key_pem) pair the rustls config builders consume.
/// Shared by the keystore load path so it can record a per-context identity
/// without changing the key's encoding label.
pub fn der_identity_to_pem(key_pkcs8_der: &[u8], chain_der: &[Vec<u8>]) -> (String, String) {
    let mut cert_pem = String::new();
    for c in chain_der {
        cert_pem.push_str(&der_to_pem("CERTIFICATE", c));
    }
    let key_pem = der_to_pem(sniff_private_key_pem_header(key_pkcs8_der), key_pkcs8_der);
    (cert_pem, key_pem)
}

/// Build a rustls client config that trusts the roots scoped to the selected
/// SSLContext, or the platform roots when no context trust is configured, and
/// optionally presents a client certificate.
///
/// Used by the native `HttpURLConnection` client path
/// (`http_url_connection.rs`, `net_phase_e.rs`) — which, unlike the
/// `SSLSocketFactory.createSocket`/`SSLEngine` path, never invokes Java's
/// `X509TrustManager.checkServerTrusted` and so never reaches
/// `x509_manager::validate_chain` on its own. When the scoped trust roots
/// carry a `RevocationConfig` (a `PKIXRevocationChecker` was attached to the
/// `TrustManagerFactory` this connection's identity/roots came from), this
/// installs `OcspAwareServerCertVerifier` so revoked server certificates are
/// still rejected on this path.
///
/// `km_ctx_key`, when present and `ctx_key_managers_table` has a non-empty
/// entry for it, takes priority over `client_identity`: instead of always
/// presenting one fixed (cert_pem, key_pem) pair, the returned config
/// consults the real Java `KeyManager.chooseClientAlias` synchronously
/// during the handshake (see `JavaKeyManagerResolver`), so a client with
/// multiple available certificates presents the one matching the server's
/// `CertificateRequest` acceptable-issuer list. Falls back to
/// `client_identity` (or no client auth) when no KeyManager was captured —
/// e.g. a plain `SSLContext.init(null, tms, null)` client with no KMF.
pub(crate) fn build_engine_client_config_with_identity(
    alpn: &[&str],
    client_identity: Option<(&str, &str)>,
    km_ctx_key: Option<u64>,
    trust_managers_ctx_key: Option<u64>,
) -> Result<Arc<ClientConfig>, String> {
    let trust_roots = active_client_trust_roots();
    let revocation = trust_roots.as_ref().and_then(|r| r.revocation.clone());
    let roots = root_store_for_trust_roots(trust_roots.as_ref());
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] build_engine_client_config_with_identity km_ctx_key={:?} client_identity_present={}",
            km_ctx_key,
            client_identity.is_some()
        );
    }
    let use_java_trust_manager = trust_managers_ctx_key
        .and_then(|key| {
            ctx_trust_managers_table()
                .lock()
                .get(&key)
                .map(|managers| (!managers.is_empty()).then_some(key))
        })
        .is_some();
    if let Some(key) = km_ctx_key {
        let has_kms = ctx_key_managers_table()
            .lock()
            .get(&key)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] build_engine_client_config_with_identity key={} has_kms={}",
                key, has_kms
            );
        }
        if has_kms {
            let resolver: Arc<dyn ResolvesClientCert> = Arc::new(JavaKeyManagerResolver {
                km_ctx_key: key,
                provider: Arc::new(cbc_augmented_default_provider()),
            });
            return build_client_config_with_revocation_and_resolver(
                roots,
                alpn,
                revocation,
                resolver,
                use_java_trust_manager,
            );
        }
    }
    build_client_config_with_revocation(
        roots,
        alpn,
        client_identity,
        revocation,
        use_java_trust_manager,
    )
}

/// Build a rustls client configuration for one explicit Java `SSLContext`.
/// This is intentionally per-call rather than using the HttpsURLConnection
/// process-wide selected-context slot: separate `HttpClient` instances are
/// allowed to carry different trust managers simultaneously.
pub(crate) fn client_config_for_ssl_context(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
) -> Result<Arc<ClientConfig>, String> {
    client_config_for_ssl_context_with_ciphers(ctx, ctx_obj, &[])
}

/// As `client_config_for_ssl_context`, but applies the Java cipher-suite
/// restriction carried by a caller's `SSLParameters`.  The JDK HttpClient
/// path creates its own rustls connection rather than an `SSLSocket`, so this
/// must be applied while building that connection's `ClientConfig`; merely
/// retaining the `SSLParameters` object on the client would not constrain the
/// TLS ClientHello.
pub(crate) fn client_config_for_ssl_context_with_ciphers(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
    enabled_ciphers: &[String],
) -> Result<Arc<ClientConfig>, String> {
    let key = ctx_obj_key(ctx, ctx_obj)
        .map_err(|_| "--jdk-only refused a class this TLS context needs".to_string())?;
    let identity = ctx_identity(ctx, ctx_obj)
        .map_err(|_| "--jdk-only refused a class this TLS context needs".to_string())?;
    build_engine_client_config_with_identity_ciphers(
        &["http/1.1"],
        identity
            .as_ref()
            .map(|(cert, key)| (cert.as_str(), key.as_str())),
        Some(key),
        Some(key),
        enabled_ciphers,
        &[],
    )
}

/// As `build_engine_client_config_with_identity`, but additionally restricts
/// the negotiable cipher suites to `enabled_ciphers` and the offered TLS
/// versions to `enabled_protocols`, each when non-empty. Used by
/// `SSLSocket.setEnabledCipherSuites`/`setEnabledProtocols` (net_phase_e.rs)
/// and by `http_url_connection`'s HTTPS connect once a caller-installed
/// `SSLSocketFactory` has been probed for its restrictions (see
/// `huc_client_tls_restrictions`) — callers rely on the JDK contract that a
/// socket rejects the handshake when restricted to a suite or protocol
/// version the server doesn't support (e.g. Tomcat's
/// `TesterSupport.ClientSSLSocketFactory`).
pub(crate) fn build_engine_client_config_with_identity_ciphers(
    alpn: &[&str],
    client_identity: Option<(&str, &str)>,
    km_ctx_key: Option<u64>,
    trust_managers_ctx_key: Option<u64>,
    enabled_ciphers: &[String],
    enabled_protocols: &[String],
) -> Result<Arc<ClientConfig>, String> {
    let trust_roots = active_client_trust_roots();
    let revocation = trust_roots.as_ref().and_then(|r| r.revocation.clone());
    let roots = root_store_for_trust_roots(trust_roots.as_ref());
    let use_java_trust_manager = trust_managers_ctx_key
        .and_then(|key| {
            ctx_trust_managers_table()
                .lock()
                .get(&key)
                .map(|managers| (!managers.is_empty()).then_some(key))
        })
        .is_some();
    let (provider, versions) = provider_and_versions(enabled_ciphers, enabled_protocols);
    if let Some(key) = km_ctx_key {
        let has_kms = ctx_key_managers_table()
            .lock()
            .get(&key)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if has_kms {
            let resolver: Arc<dyn ResolvesClientCert> = Arc::new(JavaKeyManagerResolver {
                km_ctx_key: key,
                provider: provider.clone(),
            });
            return build_client_config_ex_with_provider(
                roots,
                alpn,
                ClientAuthMode::Resolver(resolver),
                revocation,
                use_java_trust_manager,
                provider,
                &versions,
                None,
                // No engine on this path, so the pass-through verifier consults
                // nothing and the post-handshake gate does the work.
                None,
            );
        }
    }
    build_client_config_ex_with_provider(
        roots,
        alpn,
        ClientAuthMode::Fixed(client_identity),
        revocation,
        use_java_trust_manager,
        provider,
        &versions,
        None,
        None,
    )
}

// The client identity (cert_pem, key_pem) installed via
// `HttpsURLConnection.setDefaultSSLSocketFactory`. The native HttpsURLConnection
// client (`http_url_connection::perform`) does not route through
// `SSLSocketFactory.createSocket`, so it reads this global to present a client
// certificate for mTLS (e.g. `TestClientCertTls13`'s `getUrl`/`postUrl`).
static HUC_DEFAULT_CLIENT_IDENTITY: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();
static HUC_DEFAULT_TRUST_ROOTS: OnceLock<Mutex<Option<TlsTrustRoots>>> = OnceLock::new();
// A ClientConfig owns rustls's client-side session store. HttpsURLConnection
// makes a new connection for each URL request, so rebuilding this config for
// every request discards the TLS 1.3 ticket needed by the next request.
static HUC_DEFAULT_CLIENT_CONFIG: OnceLock<Mutex<Option<Arc<ClientConfig>>>> = OnceLock::new();
// Instance-level HttpsURLConnection factories must not alter a later
// connection's TLS policy. Store their finished rustls configs by Java
// connection identity; process defaults continue to use the slots above.
static HUC_CONNECTION_CLIENT_CONFIGS: OnceLock<Mutex<HashMap<i32, Arc<ClientConfig>>>> =
    OnceLock::new();

fn huc_default_identity_slot() -> &'static Mutex<Option<(String, String)>> {
    HUC_DEFAULT_CLIENT_IDENTITY.get_or_init(|| Mutex::new(None))
}

fn huc_default_trust_roots_slot() -> &'static Mutex<Option<TlsTrustRoots>> {
    HUC_DEFAULT_TRUST_ROOTS.get_or_init(|| Mutex::new(None))
}

fn huc_default_client_config_slot() -> &'static Mutex<Option<Arc<ClientConfig>>> {
    HUC_DEFAULT_CLIENT_CONFIG.get_or_init(|| Mutex::new(None))
}

fn huc_connection_client_configs() -> &'static Mutex<HashMap<i32, Arc<ClientConfig>>> {
    HUC_CONNECTION_CLIENT_CONFIGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn clear_huc_default_client_config() {
    *huc_default_client_config_slot().lock() = None;
}

pub(crate) fn set_huc_default_client_identity(ident: Option<(String, String)>) {
    clear_huc_default_client_config();
    *huc_default_identity_slot().lock() = ident;
    let roots = selected_context_trust_roots();
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] set_huc_default_client_identity capturing roots={:?}",
            roots.as_ref().map(|r| r.root_ders.len())
        );
    }
    *huc_default_trust_roots_slot().lock() = roots;
    set_selected_context_trust_roots(None);
}

pub fn huc_default_client_identity() -> Option<(String, String)> {
    huc_default_identity_slot().lock().clone()
}

fn huc_default_trust_roots() -> Option<TlsTrustRoots> {
    huc_default_trust_roots_slot().lock().clone()
}

/// The `ctx_key_managers_table` key of the `SSLContext` that most recently
/// supplied `HUC_DEFAULT_CLIENT_IDENTITY`. Set alongside that identity (same
/// `getSocketFactory()` capture point — see its call site) so
/// `http_url_connection::perform`'s client config can consult the real
/// `KeyManager.chooseClientAlias` (via `JavaKeyManagerResolver`) instead of
/// only ever presenting the one fixed identity `ctx_identity` captured. A
/// plain `u64` — never the `KeyManager` `ObjectRef`s themselves, which stay
/// solely in the GC-rooted `ctx_key_managers_table` (see that table's doc).
static HUC_DEFAULT_KM_CTX_KEY: OnceLock<Mutex<Option<u64>>> = OnceLock::new();
static HUC_DEFAULT_TM_CTX_KEY: OnceLock<Mutex<Option<u64>>> = OnceLock::new();

fn huc_default_km_ctx_key_slot() -> &'static Mutex<Option<u64>> {
    HUC_DEFAULT_KM_CTX_KEY.get_or_init(|| Mutex::new(None))
}

fn huc_default_tm_ctx_key_slot() -> &'static Mutex<Option<u64>> {
    HUC_DEFAULT_TM_CTX_KEY.get_or_init(|| Mutex::new(None))
}

pub(crate) fn set_huc_default_key_managers_ctx_key(key: Option<u64>) {
    clear_huc_default_client_config();
    *huc_default_km_ctx_key_slot().lock() = key;
}

pub(crate) fn huc_default_key_managers_ctx_key() -> Option<u64> {
    *huc_default_km_ctx_key_slot().lock()
}

fn capture_huc_trust_managers_ctx_key(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let has_managers = ctx_trust_managers_table()
        .lock()
        .get(&key)
        .map(|managers| !managers.is_empty())
        .unwrap_or(false);
    *huc_default_tm_ctx_key_slot().lock() = has_managers.then_some(key);
    Ok(())
}

pub(crate) fn huc_default_trust_managers_ctx_key() -> Option<u64> {
    *huc_default_tm_ctx_key_slot().lock()
}

/// Pins `ctx_obj` across [`capture_huc_key_managers_ctx_key_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `ctx_obj` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub(crate) fn capture_huc_key_managers_ctx_key(
    ctx: &mut dyn NativeContext,
    ctx_obj: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let w5_pin = ctx.pin_native_root(*ctx_obj);
    let w5_out = capture_huc_key_managers_ctx_key_body(ctx, *ctx_obj);
    *ctx_obj = ctx.read_native_pin(w5_pin, *ctx_obj);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// `SSLContext.getSocketFactory()` calls this alongside
/// `set_huc_default_client_identity` (same reliable per-context capture
/// point — see that call site's doc). Clears the slot when this context has
/// no captured `KeyManager`s at all, so a plain non-mTLS client (or one
/// whose `SSLContext.init` passed a null/empty `KeyManager[]`) keeps falling
/// back to `client_identity`/no-client-auth instead of spuriously trying (and
/// failing) to consult an empty resolver.
pub(crate) fn capture_huc_key_managers_ctx_key_body(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let has_kms = ctx_key_managers_table().lock().contains_key(&key);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] capture_huc_key_managers_ctx_key key={} has_kms={}",
            key, has_kms
        );
    }
    set_huc_default_key_managers_ctx_key(if has_kms { Some(key) } else { None });
    Ok(())
}

/// Capture all TLS state for the Java SSLContext supplying HttpsURLConnection.
/// This also runs for anonymous clients: `ctx_identity` transfers scoped trust
/// roots even when it returns no client certificate, and every context needs a
/// stable ClientConfig to retain TLS 1.3 tickets across URL requests.
pub(crate) fn capture_huc_ssl_context(
    ctx: &mut dyn NativeContext,
    ctx_obj: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let ident = ctx_identity(ctx, *ctx_obj)?;
    set_huc_default_client_identity(ident);
    capture_huc_key_managers_ctx_key(ctx, ctx_obj)?;
    capture_huc_trust_managers_ctx_key(ctx, *ctx_obj)?;

    let ident = huc_default_client_identity();
    let km_ctx_key = huc_default_key_managers_ctx_key();
    let trust_managers_ctx_key = huc_default_trust_managers_ctx_key();
    let config = build_engine_client_config_with_identity(
        &["http/1.1"],
        ident
            .as_ref()
            .map(|(cert, key)| (cert.as_str(), key.as_str())),
        km_ctx_key,
        trust_managers_ctx_key,
    );
    *huc_default_client_config_slot().lock() = config.ok();
    Ok(())
}

/// Capture an instance factory without changing the process-default TLS
/// policy. HttpsURLConnection's instance setter is scoped to one connection;
/// keeping its config in the default slot made a later plain connection accept
/// the previous connection's permissive TrustManager.
pub(crate) fn capture_huc_ssl_context_for_connection(
    ctx: &mut dyn NativeContext,
    connection: ObjectRef,
    ctx_obj: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let default_identity = huc_default_identity_slot().lock().clone();
    let default_roots = huc_default_trust_roots_slot().lock().clone();
    let default_config = huc_default_client_config_slot().lock().clone();
    let default_km = *huc_default_km_ctx_key_slot().lock();
    let default_tm = *huc_default_tm_ctx_key_slot().lock();

    capture_huc_ssl_context(ctx, ctx_obj)?;
    if let Some(config) = huc_default_client_config() {
        let key = ctx.identity_hash_code(connection);
        let mut configs = huc_connection_client_configs().lock();
        if configs.len() >= 256 {
            configs.clear();
        }
        configs.insert(key, config);
    }

    *huc_default_identity_slot().lock() = default_identity;
    *huc_default_trust_roots_slot().lock() = default_roots;
    *huc_default_client_config_slot().lock() = default_config;
    *huc_default_km_ctx_key_slot().lock() = default_km;
    *huc_default_tm_ctx_key_slot().lock() = default_tm;
    Ok(())
}

/// Returns the shared HttpsURLConnection config selected by its SSLContext.
/// Cloning the Arc intentionally shares rustls's session-resumption store.
pub(crate) fn huc_default_client_config() -> Option<Arc<ClientConfig>> {
    huc_default_client_config_slot().lock().clone()
}

pub(crate) fn huc_client_config_for_connection(
    ctx: &dyn NativeContext,
    connection: ObjectRef,
) -> Option<Arc<ClientConfig>> {
    huc_connection_client_configs()
        .lock()
        .get(&ctx.identity_hash_code(connection))
        .cloned()
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

// -----------------------------------------------------------------------------
// EC PKCS#8 v1 repair: recover the missing public key from the leaf certificate
// -----------------------------------------------------------------------------
//
// `ring` (and therefore rustls's ring backend, including the vendored
// `rustls-cbc`) can only build an `EcdsaKeyPair` from a PKCS#8 document whose
// inner SEC1 `ECPrivateKey` carries the optional `publicKey [1]` BIT STRING.
// The SEC1 branch is no escape hatch: `EcdsaSigningKey::convert_sec1_to_pkcs8`
// re-wraps and calls the same `from_pkcs8`.
//
// `java.security.KeyPairGenerator("EC")` emits the OTHER shape — inner SEC1 of
// just `version` + `privateKey`, no `parameters [0]`, no `publicKey [1]`. That
// is not a CratonVM quirk: HotSpot's SunEC produces a byte-identical 67-byte
// P-256 encoding (verified against JDK 25). rustls reports the refusal as the
// generic "failed to parse private key as RSA, ECDSA, or EdDSA", which reads
// like a corrupt key and is not — see
// `docs/known-issues/netty/ec-pkcs8-v1-server-identity-rejected-20260812.md`.
//
// Rather than derive the public point (the in-tree `crypto_impl` EC core is
// P-256 only, so that would fix one curve), take it from the leaf
// certificate's `SubjectPublicKeyInfo`, which by definition holds the public
// key for this identity. That is curve-agnostic and needs no EC arithmetic.
// It is also self-checking: `ring`'s `from_pkcs8` recomputes the public key
// from the private scalar and rejects the document if the two disagree, so a
// cert/key mismatch fails closed exactly as before rather than producing an
// identity that signs with the wrong key.

/// Byte-length of the DER TLV header at `at` (identifier octet + length
/// octets), or `None` if the header is truncated or uses an unsupported
/// long form.
fn der_header_len(buf: &[u8], at: usize) -> Option<usize> {
    let len_byte = *buf.get(at + 1)?;
    if len_byte & 0x80 == 0 {
        return Some(2);
    }
    let n = (len_byte & 0x7f) as usize;
    // Indefinite length (n == 0) is not valid DER; > 4 length octets is far
    // beyond anything in a key or certificate.
    if n == 0 || n > 4 {
        return None;
    }
    Some(2 + n)
}

/// Value length of the TLV at `at`.
fn der_value_len(buf: &[u8], at: usize) -> Option<usize> {
    let len_byte = *buf.get(at + 1)?;
    if len_byte & 0x80 == 0 {
        return Some(len_byte as usize);
    }
    let n = (len_byte & 0x7f) as usize;
    if n == 0 || n > 4 {
        return None;
    }
    let mut len = 0usize;
    for i in 0..n {
        len = len
            .checked_mul(256)?
            .checked_add(*buf.get(at + 2 + i)? as usize)?;
    }
    Some(len)
}

/// `(value_start, value_end)` of the TLV at `at`, bounds-checked against `buf`.
fn der_tlv(buf: &[u8], at: usize) -> Option<(usize, usize)> {
    let hdr = der_header_len(buf, at)?;
    let len = der_value_len(buf, at)?;
    let start = at.checked_add(hdr)?;
    let end = start.checked_add(len)?;
    (end <= buf.len()).then_some((start, end))
}

/// End offset (exclusive) of the whole TLV at `at`.
fn der_tlv_end(buf: &[u8], at: usize) -> Option<usize> {
    Some(der_tlv(buf, at)?.1)
}

/// `1.2.840.10045.2.1` (id-ecPublicKey), as it appears inside an
/// `AlgorithmIdentifier` — tag, length and contents.
const OID_ID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];

/// Encode one DER TLV.
fn der_tlv_encode(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(tag);
    let n = body.len();
    if n < 0x80 {
        out.push(n as u8);
    } else if n <= 0xff {
        out.extend_from_slice(&[0x81, n as u8]);
    } else {
        out.extend_from_slice(&[0x82, (n >> 8) as u8, (n & 0xff) as u8]);
    }
    out.extend_from_slice(body);
    out
}

/// Pull the `subjectPublicKey` BIT STRING contents (the uncompressed EC point,
/// `0x04 || X || Y`) out of a DER certificate's `SubjectPublicKeyInfo`, if the
/// certificate carries an EC public key.
///
/// Walks `Certificate -> tbsCertificate -> *` looking for the child SEQUENCE
/// shaped `{ AlgorithmIdentifier{ id-ecPublicKey, .. }, BIT STRING }`. Matching
/// on shape rather than counting fields keeps this correct across the optional
/// `[0] version` and the optional trailing extension fields.
fn cert_ec_public_key_bits(cert_der: &[u8]) -> Option<Vec<u8>> {
    if *cert_der.first()? != 0x30 {
        return None;
    }
    let (cert_body, cert_end) = der_tlv(cert_der, 0)?;
    // tbsCertificate is the first child.
    if *cert_der.get(cert_body)? != 0x30 {
        return None;
    }
    let (tbs_body, tbs_end) = der_tlv(cert_der, cert_body)?;
    if tbs_end > cert_end {
        return None;
    }
    let mut p = tbs_body;
    while p < tbs_end {
        let end = der_tlv_end(cert_der, p)?;
        if cert_der[p] == 0x30 {
            if let Some(bits) = spki_ec_bits(cert_der, p) {
                return Some(bits);
            }
        }
        p = end;
    }
    None
}

/// If the SEQUENCE at `at` is an EC `SubjectPublicKeyInfo`, return its
/// `subjectPublicKey` bits with the BIT STRING's leading unused-bits octet
/// removed.
fn spki_ec_bits(buf: &[u8], at: usize) -> Option<Vec<u8>> {
    let (body, end) = der_tlv(buf, at)?;
    // child 1: AlgorithmIdentifier SEQUENCE starting with id-ecPublicKey
    if *buf.get(body)? != 0x30 {
        return None;
    }
    let (alg_body, alg_end) = der_tlv(buf, body)?;
    if buf.get(alg_body..alg_body.checked_add(OID_ID_EC_PUBLIC_KEY.len())?)? != OID_ID_EC_PUBLIC_KEY
    {
        return None;
    }
    // child 2: subjectPublicKey BIT STRING
    if alg_end >= end || *buf.get(alg_end)? != 0x03 {
        return None;
    }
    let (bits_body, bits_end) = der_tlv(buf, alg_end)?;
    // First content octet of a BIT STRING is the unused-bit count; an EC point
    // is whole octets, so it must be zero.
    if *buf.get(bits_body)? != 0 || bits_end <= bits_body + 1 {
        return None;
    }
    Some(buf[bits_body + 1..bits_end].to_vec())
}

/// Given a PKCS#8 EC private key whose inner SEC1 omits `publicKey [1]`, return
/// an equivalent PKCS#8 with the public key from `leaf_cert_der` spliced in.
///
/// Returns `None` — meaning "use the key unchanged" — when the key is not an EC
/// PKCS#8, when it already carries a public key, or when the certificate has no
/// EC public key to lend. Every failure path is a no-op, so this can only make
/// more identities usable, never fewer.
pub(crate) fn ec_pkcs8_splice_public_key(key_der: &[u8], leaf_cert_der: &[u8]) -> Option<Vec<u8>> {
    if *key_der.first()? != 0x30 {
        return None;
    }
    let (outer_body, outer_end) = der_tlv(key_der, 0)?;
    // version INTEGER (PKCS#8 v1 == 0)
    if *key_der.get(outer_body)? != 0x02 {
        return None;
    }
    let version_end = der_tlv_end(key_der, outer_body)?;
    // privateKeyAlgorithm AlgorithmIdentifier — must be id-ecPublicKey.
    if *key_der.get(version_end)? != 0x30 {
        return None;
    }
    let (alg_body, alg_end) = der_tlv(key_der, version_end)?;
    if key_der.get(alg_body..alg_body.checked_add(OID_ID_EC_PUBLIC_KEY.len())?)?
        != OID_ID_EC_PUBLIC_KEY
    {
        return None;
    }
    // privateKey OCTET STRING wrapping the SEC1 ECPrivateKey.
    if *key_der.get(alg_end)? != 0x04 {
        return None;
    }
    let (oct_body, oct_end) = der_tlv(key_der, alg_end)?;
    if oct_end > outer_end || *key_der.get(oct_body)? != 0x30 {
        return None;
    }
    let (sec1_body, sec1_end) = der_tlv(key_der, oct_body)?;
    // inner: version INTEGER, privateKey OCTET STRING, then optionals.
    if *key_der.get(sec1_body)? != 0x02 {
        return None;
    }
    let sec1_version_end = der_tlv_end(key_der, sec1_body)?;
    if *key_der.get(sec1_version_end)? != 0x04 {
        return None;
    }
    let sec1_priv_end = der_tlv_end(key_der, sec1_version_end)?;
    // Already has `publicKey [1]`? Then ring is happy and there is nothing to do.
    let mut p = sec1_priv_end;
    while p < sec1_end {
        if key_der[p] == 0xa1 {
            return None;
        }
        p = der_tlv_end(key_der, p)?;
    }

    let public_bits = cert_ec_public_key_bits(leaf_cert_der)?;

    // Rebuild the inner SEC1 as version + privateKey + [1] publicKey, keeping
    // any `parameters [0]` that was present. `parameters` stays optional: the
    // openssl-produced PKCS#8 that ring accepts omits it too (the curve is
    // already named by the outer AlgorithmIdentifier).
    let mut inner = Vec::new();
    inner.extend_from_slice(&key_der[sec1_body..sec1_priv_end]);
    let mut q = sec1_priv_end;
    while q < sec1_end {
        let end = der_tlv_end(key_der, q)?;
        if key_der[q] == 0xa0 {
            inner.extend_from_slice(&key_der[q..end]);
        }
        q = end;
    }
    let mut bit_string = Vec::with_capacity(public_bits.len() + 1);
    bit_string.push(0); // unused bits
    bit_string.extend_from_slice(&public_bits);
    inner.extend_from_slice(&der_tlv_encode(0xa1, &der_tlv_encode(0x03, &bit_string)));

    let mut outer = Vec::new();
    outer.extend_from_slice(&key_der[outer_body..version_end]); // version
    outer.extend_from_slice(&key_der[version_end..alg_end]); // privateKeyAlgorithm
    outer.extend_from_slice(&der_tlv_encode(0x04, &der_tlv_encode(0x30, &inner)));
    Some(der_tlv_encode(0x30, &outer))
}

/// Apply [`ec_pkcs8_splice_public_key`] to a parsed key when the chain's leaf
/// can supply the missing public key. A no-op for every other key shape.
pub(crate) fn repair_ec_key_for_ring<'a>(
    key: PrivateKeyDer<'a>,
    chain: &[CertificateDer<'_>],
) -> PrivateKeyDer<'a> {
    let PrivateKeyDer::Pkcs8(ref pkcs8) = key else {
        return key;
    };
    let Some(leaf) = chain.first() else {
        return key;
    };
    match ec_pkcs8_splice_public_key(pkcs8.secret_pkcs8_der(), leaf.as_ref()) {
        Some(repaired) => {
            if crate::nbflags().dbg_tls_hs {
                eprintln!(
                    "[dbg-tls-hs] repair_ec_key_for_ring: spliced cert public key into EC PKCS#8 \
                     ({} -> {} bytes)",
                    pkcs8.secret_pkcs8_der().len(),
                    repaired.len()
                );
            }
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(repaired))
        }
        None => key,
    }
}

/// Build a `CertifiedKey` from raw DER, repairing a JDK-shaped EC key first.
///
/// The mTLS `KeyManager` resolver is the one identity path that never sees PEM:
/// its material arrives as DER from `km_alias_material`. That is why it was the
/// call site the EC repair originally missed — it does not share a line with the
/// six `parse_private_key_pem` builders. It exists as a named function, rather
/// than three lines inlined into `resolve_via_java`, so a test can exercise the
/// path the resolver actually takes: delete the repair here and
/// `a_key_manager_supplied_jdk_ec_identity_is_repaired_too` goes red.
pub(crate) fn certified_key_from_der_repairing_ec(
    cert_chain: Vec<CertificateDer<'static>>,
    key_der: Vec<u8>,
    provider: &rustls::crypto::CryptoProvider,
) -> Result<CertifiedKey, rustls::Error> {
    let key = repair_ec_key_for_ring(
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
        &cert_chain,
    );
    CertifiedKey::from_der(cert_chain, key, provider)
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
// Keystore -> runtime identity / scoped trust bridge (JSSE server connector)
// -----------------------------------------------------------------------------
//
// Tomcat's JSSE connector loads its server cert/key from a JKS/PKCS12 keystore
// and its trust anchors from a truststore, then drives the rustls-backed
// SSLEngine. The keystore natives (keystore.rs) parse identity DER into the PEM
// the rustls config builders consume. Trust anchors flow through
// TrustManagerFactory -> SSLContext via `set_pending_tm_trust_roots`, so a
// configured custom truststore remains scoped and restrictive.

/// Standard base64 (RFC 4648) encoder — dependency-free so we don't add a crate
/// just to render DER as PEM.
fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Wrap DER bytes in a PEM block with 64-char base64 lines.
fn der_to_pem(label: &str, der: &[u8]) -> String {
    let b64 = b64_encode(der);
    let mut body = String::new();
    for line in b64.as_bytes().chunks(64) {
        body.push_str(std::str::from_utf8(line).unwrap_or(""));
        body.push('\n');
    }
    format!("-----BEGIN {label}-----\n{body}-----END {label}-----\n")
}

/// Legacy entry point kept for older callers. Keystore loads must not mutate a
/// process-wide TLS root set; trust roots are scoped through
/// `set_pending_tm_trust_roots` and the following `SSLContext.init`.
pub fn add_extra_trust_root_der(_der: Vec<u8>) {}

fn root_store_for_trust_roots(trust_roots: Option<&TlsTrustRoots>) -> RootCertStore {
    let mut roots = match trust_roots {
        Some(_) => RootCertStore::empty(),
        None => load_native_root_store().unwrap_or_else(|_| RootCertStore::empty()),
    };
    if let Some(bundle) = trust_roots {
        for der in &bundle.root_ders {
            let _ = roots.add(CertificateDer::from(der.clone()));
        }
    }
    roots
}

fn active_client_trust_roots() -> Option<TlsTrustRoots> {
    let selected = take_selected_context_trust_roots();
    let result = selected.or_else(huc_default_trust_roots);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] active_client_trust_roots -> {:?}",
            result.as_ref().map(|r| r.root_ders.len())
        );
    }
    result
}

/// Concatenate scoped trust anchors (DER) into a PEM bundle. Used as the
/// client-CA source for mTLS server configs when no explicit `client_ca_pem`
/// was installed on the runtime identity.
fn trust_roots_pem(trust_roots: Option<&TlsTrustRoots>) -> String {
    let mut pem = String::new();
    if let Some(bundle) = trust_roots {
        for der in &bundle.root_ders {
            pem.push_str(&der_to_pem("CERTIFICATE", der));
        }
    }
    pem
}

/// Best-effort sniff of a raw private-key DER's actual ASN.1 encoding.
/// `PrivateKey.getEncoded()` is NOT guaranteed to be PKCS#8 for every JCA
/// provider/key type -- observed live: BouncyCastle-generated self-signed
/// server identities intermittently hand back a raw PKCS#1 (RSA) or SEC1
/// (EC) encoding rather than a PKCS#8-wrapped one. Blindly labeling every
/// DER as PKCS#8 (the prior behavior) makes rustls's `with_single_cert`
/// fail with a generic "failed to parse private key as RSA, ECDSA, or
/// EdDSA" the moment the actual bytes aren't PKCS#8 -- this is the root
/// cause of the intermittent `ServerHttpsRequestIntegrationTests` TLS
/// handshake failure ("unexpected EOF"): `engine_begin` silently returned
/// `Err` before ever touching the network, and Netty's `SslHandler`
/// reacted to the resulting broken engine by tearing the connection down
/// on the very first `unwrap()`.
///
/// Distinguishes the three shapes by looking at what immediately follows
/// the outer SEQUENCE's version INTEGER: a nested SEQUENCE means a PKCS#8
/// `AlgorithmIdentifier` follows; a nested OCTET STRING means a SEC1
/// `ECPrivateKey.privateKey` field; anything else (typically another
/// INTEGER, RSA's modulus `n`) means PKCS#1. Defaults to PKCS#8 (the prior
/// hardcoded assumption) if the DER doesn't parse as expected, so this is
/// strictly additive -- it can only recognize MORE valid keys, never fewer.
fn sniff_private_key_pem_header(der: &[u8]) -> &'static str {
    fn tlv_bounds(buf: &[u8], tag_pos: usize) -> Option<(usize, usize)> {
        if tag_pos + 1 >= buf.len() {
            return None;
        }
        let len_byte = buf[tag_pos + 1];
        let mut p = tag_pos + 2;
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
        let end = p.checked_add(len)?;
        (end <= buf.len()).then_some((p, end))
    }
    (|| -> Option<&'static str> {
        if *der.first()? != 0x30 {
            return None; // must be a top-level SEQUENCE
        }
        let (after_outer, _) = tlv_bounds(der, 0)?;
        if *der.get(after_outer)? != 0x02 {
            return None; // version INTEGER
        }
        let (_, after_version) = tlv_bounds(der, after_outer)?;
        match der.get(after_version) {
            Some(0x30) => Some("PRIVATE KEY"),    // PKCS#8 AlgorithmIdentifier
            Some(0x04) => Some("EC PRIVATE KEY"), // SEC1 privateKey OCTET STRING
            Some(0x02) => Some("RSA PRIVATE KEY"), // PKCS#1 modulus INTEGER
            _ => None,
        }
    })()
    .unwrap_or("PRIVATE KEY")
}

/// Install the runtime server identity from a private key DER (PKCS#8,
/// PKCS#1, or SEC1 -- auto-detected, see `sniff_private_key_pem_header`) +
/// DER cert chain (leaf first), converting to the PEM the rustls
/// server-config builder consumes. Called from the keystore load path when
/// a key entry is present.
pub fn install_identity_from_der(key_pkcs8_der: &[u8], chain_der: &[Vec<u8>]) {
    if key_pkcs8_der.is_empty() || chain_der.is_empty() {
        return;
    }
    let mut cert_pem = String::new();
    for c in chain_der {
        cert_pem.push_str(&der_to_pem("CERTIFICATE", c));
    }
    let __sniffed = sniff_private_key_pem_header(key_pkcs8_der);
    if crate::nbflags().dbg_tls_hs {
        eprintln!(
            "[dbg-tls-hs] install_identity_from_der: key_der_len={} full_hex={} sniffed_header={}",
            key_pkcs8_der.len(),
            key_pkcs8_der
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>(),
            __sniffed
        );
    }
    let key_pem = der_to_pem(__sniffed, key_pkcs8_der);

    // Validate-before-overwrite (http-server-sslengine-identity-singleton-clobber):
    // the process-global `RUNTIME_TLS_IDENTITY` is last-write-wins, so a
    // `KeyStore.setKeyEntry` on an UNRELATED keystore (never wired to the
    // server's `SSLContext`) used to clobber a working server identity with a
    // malformed/mismatched one, breaking every subsequent handshake. If a
    // usable identity is already installed, only overwrite it when the NEW
    // (cert, key) pair actually builds a rustls `ServerConfig` — i.e. the key
    // parses and matches its leaf cert. A new identity that fails to build is
    // rejected (the working one is kept) with a warning; if no identity is
    // installed yet, we install unconditionally (nothing to protect, and a
    // first identity that later proves unusable surfaces its own error at
    // handshake time exactly as before).
    let candidate_builds =
        build_server_config_single_cert(&cert_pem, &key_pem, &["http/1.1"], false, None).is_ok();
    if !candidate_builds {
        if runtime_tls_identity().is_some() {
            eprintln!(
                "[cratonvm-tls] rejecting setKeyEntry TLS identity that does not build a \
                 valid ServerConfig (key parse/cert-mismatch); keeping the previously \
                 installed identity (see http-server-sslengine-identity-singleton-clobber)"
            );
            return;
        }
        // No prior identity: fall through and install anyway — preserves the
        // pre-fix behavior where the failure (if this identity is ever used)
        // is reported at handshake time, and avoids regressing any path that
        // stages an identity the native config-builder happens to reject but
        // that some other code path repairs.
    }

    set_runtime_tls_identity(Some(RuntimeTlsIdentity {
        cert_pem,
        key_pem,
        client_ca_pem: None,
    }));
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

/// Build **this side's own** TLS identity from a certificate chain and its
/// private key.
///
/// Deliberately `CertifiedKey::new` and not `CertifiedKey::from_der`. The
/// difference is one call — `from_der` additionally runs `keys_match()`, which
/// parses the end-entity certificate through webpki purely to compare its
/// `SubjectPublicKeyInfo` against the private key's:
///
/// ```text
/// pub fn keys_match(&self) -> Result<(), Error> {
///     let Some(key_spki) = self.key.public_key() else {
///         return Err(InconsistentKeys::Unknown.into());   // <- already tolerated
///     };
///     let cert = ParsedCertificate::try_from(self.end_entity_cert()?)?;
///     match key_spki == cert.subject_public_key_info() { … }
/// }
/// ```
///
/// webpki's parser accepts only v3 certificates (`cert.rs`'s `version3`), so a
/// **v1** identity fails that parse, and the failure escapes as
/// `InvalidCertificate(Other(UnsupportedCertVersion))` instead of landing in
/// the `InconsistentKeys::Unknown` arm the surrounding code already treats as
/// "cannot tell, carry on". The result was that this VM would not *present* a
/// v1 certificate at all — an error raised while building a config, before any
/// peer or trust decision exists. JSSE has no such restriction, and netty's
/// mutual-auth fixtures are all v1 end-entity certificates, so 72 of
/// `JdkSslEngineTest`'s failures were this one check.
///
/// What is given up is the early "your certificate and private key do not
/// match" diagnosis; a genuine mismatch now fails at handshake time instead of
/// config time. That check is best-effort in rustls itself — a key provider
/// that cannot expose a public key already skips it — and this module's own
/// `SniCertResolver` has always built its identity this way, so the five
/// single-certificate paths were the odd ones out rather than the safe ones.
///
/// This does NOT touch peer verification: a peer's chain still goes through
/// webpki path building, v3 rule included.
fn identity_certified_key(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<CertifiedKey, String> {
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| format!("unsupported private key: {}", e))?;
    Ok(CertifiedKey::new(chain, signing_key))
}

impl SniCertResolver {
    /// Build a `CertifiedKey` from PEM blobs. Uses rustls's ring-backed
    /// signer, which covers RSA 2048/3072/4096 and ECDSA P-256/P-384.
    fn certified_key_from_pem(cert_pem: &str, key_pem: &str) -> Result<Arc<CertifiedKey>, String> {
        let chain = parse_cert_chain_pem(cert_pem)?;
        // Same repair as the non-SNI server builder: a JDK EC key arrives
        // without the `publicKey [1]` ring needs, and the cert carries it.
        let key = repair_ec_key_for_ring(parse_private_key_pem(key_pem)?, &chain);
        Ok(Arc::new(identity_certified_key(chain, key)?))
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
    pub(crate) config: TlsServerConfig,
    pub(crate) local_port: u16,
    /// Set by `rustls_listener_close` and polled by `rustls_server_accept`'s
    /// non-blocking accept loop (see that function's doc comment) — lets a
    /// concurrent `close()` unblock a thread parked in `accept()` without
    /// needing the `sreg()` mutex, which that thread cannot hold while
    /// blocked.
    pub(crate) closed: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(crate) enum TlsServerConfig {
    Rustls(Arc<ServerConfig>),
    Native(native_tls::TlsAcceptor),
    #[cfg(unix)]
    LegacyDsa(SslAcceptor),
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
    /// Per-stream mutex, NOT guarded by `sreg()`. Blocking socket I/O on one
    /// TLS connection must never hold the process-wide registry lock — see
    /// `rustls_stream_read`'s doc comment.
    pub(crate) stream: Arc<Mutex<StreamOwned<ClientConnection, TcpStream>>>,
    /// A `try_clone`d handle on the same socket, reachable WITHOUT `stream`'s
    /// mutex. Added 2026-08-12 (W7-61), modelled exactly on
    /// `servlet::TlsEntry::raw`, which already existed for this purpose on the
    /// native-tls table. It is the only way `rustls_stream_close` can reach the
    /// socket while another thread is parked in a read on it — that thread
    /// holds both the mutex and an `Arc`, so neither `try_lock` nor dropping
    /// our own `Arc` touches the connection. `None` only if `try_clone` failed.
    pub(crate) raw: Option<TcpStream>,
    pub(crate) peer_host: String,
    pub(crate) peer_port: u16,
    pub(crate) negotiated_protocol: String,
    pub(crate) negotiated_cipher: String,
    pub(crate) negotiated_alpn: Option<String>,
}

pub(crate) struct TlsServerStreamEntry {
    /// Per-stream mutex — see `TlsClientStreamEntry::stream`.
    pub(crate) stream: Arc<Mutex<TlsServerStream>>,
    /// See `TlsClientStreamEntry::raw`.
    pub(crate) raw: Option<TcpStream>,
    pub(crate) sni_hostname: Option<String>,
    pub(crate) negotiated_protocol: String,
    pub(crate) negotiated_cipher: String,
    pub(crate) negotiated_alpn: Option<String>,
}

pub(crate) enum TlsServerStream {
    Rustls(StreamOwned<ServerConnection, TcpStream>),
    Native(native_tls::TlsStream<TcpStream>),
    #[cfg(unix)]
    LegacyDsa(openssl::ssl::SslStream<TcpStream>),
    /// The TCP connection was accepted; its TLS handshake was NOT completed.
    ///
    /// JSSE does not run the handshake inside `SSLServerSocket.accept()` at
    /// all — `accept()` returns as soon as the TCP connection is up, and the
    /// handshake runs on the returned socket's first read or write. A peer
    /// that connects and disconnects without a ClientHello therefore costs
    /// HotSpot one accepted socket whose first read throws
    /// `SSLHandshakeException`; the listener is untouched. This variant is how
    /// [`rustls_server_accept`] reaches the same end state while still running
    /// the handshake eagerly: the failure is carried ON the accepted stream
    /// and raised at the first I/O, instead of being thrown out of `accept()`
    /// where it kills the caller's accept loop. See
    /// `rustls_server_handshake_failure`.
    HandshakeFailed {
        /// A duplicate handle taken BEFORE the handshake consumed the stream:
        /// the failing backends do not all hand the socket back.
        tcp: Option<TcpStream>,
        reason: String,
    },
}

impl TlsServerStream {
    /// The underlying TCP socket, borrowed. Used only to `try_clone` a
    /// registry-held duplicate at registration time — see
    /// `TlsClientStreamEntry::raw`.
    fn tcp(&self) -> Option<&TcpStream> {
        match self {
            TlsServerStream::Rustls(s) => Some(&s.sock),
            TlsServerStream::Native(s) => Some(s.get_ref()),
            #[cfg(unix)]
            TlsServerStream::LegacyDsa(s) => Some(s.get_ref()),
            TlsServerStream::HandshakeFailed { tcp, .. } => tcp.as_ref(),
        }
    }
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
    build_server_config_single_cert_ex(
        cert_pem,
        key_pem,
        alpn_protocols,
        require_client_cert,
        false,
        client_ca_pem,
    )
}

/// As `build_server_config_single_cert`, but with an explicit `optional_client_cert`
/// mode. When `optional_client_cert` is set (and `require_client_cert` is not),
/// the server still sends a `CertificateRequest` and validates/stores a client
/// cert if one is presented, but does NOT reject clients that omit it — this is
/// the JSSE `setWantClientAuth(true)` / `certificateVerification="optional"`
/// behavior. Without it, an "optional" server never asks for the cert, so
/// `conn.peer_certificates()` is empty and client-cert auth (e.g.
/// `TestClientCertTls13`) returns HTTP 401.
pub(crate) fn build_server_config_single_cert_ex(
    cert_pem: &str,
    key_pem: &str,
    alpn_protocols: &[&str],
    require_client_cert: bool,
    optional_client_cert: bool,
    client_ca_pem: Option<&str>,
) -> Result<Arc<ServerConfig>, String> {
    build_server_config_single_cert_ex_ciphers(
        cert_pem,
        key_pem,
        alpn_protocols,
        require_client_cert,
        optional_client_cert,
        client_ca_pem,
        &[],
        &[],
    )
}

/// Start a rustls `ServerConfig` builder whose accepted TLS versions honour
/// `enabled_protocols` (Java `SSLEngine.setEnabledProtocols` names, which
/// Tomcat feeds from `SSLHostConfig.protocols`). An empty/unmappable list
/// keeps rustls's safe defaults — see `protocol_versions_for`.
///
/// FIX (tls-handshake-enforcement-gap, doc 21): every server config built
/// here previously hard-coded `with_safe_default_protocol_versions()`, so a
/// connector configured for exactly one TLS version happily accepted the
/// other. `TestSSLHostConfigProtocol`'s `testTlsVersionMismatch*` cases
/// (server TLSv1.3-only vs client TLSv1.2-only, and the reverse) expect
/// `SSLHandshakeException`; both sides silently negotiated the version they
/// had in common instead.
fn server_builder_with_versions(
    enabled_ciphers: &[String],
    enabled_protocols: &[String],
) -> Result<rustls::ConfigBuilder<ServerConfig, rustls::WantsVerifier>, String> {
    let (provider, versions) = provider_and_versions(enabled_ciphers, enabled_protocols);
    ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&versions)
        .map_err(|e| format!("with_protocol_versions failed: {}", e))
}

/// As `build_server_config_single_cert_ex`, but restricts the negotiable
/// cipher suites to `enabled_ciphers` (Java `SSLEngine.setEnabledCipherSuites`
/// names) and the accepted TLS versions to `enabled_protocols` when non-empty
/// — empty lists keep the unrestricted `ring` default and rustls's safe
/// default versions, identical to `build_server_config_single_cert_ex`.
pub(crate) fn build_server_config_single_cert_ex_ciphers(
    cert_pem: &str,
    key_pem: &str,
    alpn_protocols: &[&str],
    require_client_cert: bool,
    optional_client_cert: bool,
    client_ca_pem: Option<&str>,
    enabled_ciphers: &[String],
    enabled_protocols: &[String],
) -> Result<Arc<ServerConfig>, String> {
    let chain = parse_cert_chain_pem(cert_pem)?;
    // JDK-generated EC keys omit the `publicKey [1]` that ring demands; the
    // leaf certificate carries it. No-op for every other key shape.
    let key = repair_ec_key_for_ring(parse_private_key_pem(key_pem)?, &chain);

    let builder = server_builder_with_versions(enabled_ciphers, enabled_protocols)?;
    let builder = if require_client_cert || optional_client_cert {
        let ca_pem = client_ca_pem
            .ok_or_else(|| "client auth requested but client_ca_pem is None".to_string())?;
        let mut roots = RootCertStore::empty();
        for cert in parse_cert_chain_pem(ca_pem)? {
            roots
                .add(cert)
                .map_err(|e| format!("client CA add failed: {}", e))?;
        }
        let vb = WebPkiClientVerifier::builder(Arc::new(roots));
        let verifier = if require_client_cert {
            vb.build()
        } else {
            vb.allow_unauthenticated().build()
        }
        .map_err(|e| format!("client verifier build failed: {}", e))?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };

    // `with_single_cert` is exactly `CertifiedKey::from_der` + this resolver;
    // the only difference is the `keys_match` parse. See
    // `identity_certified_key`.
    let mut config = builder.with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
        identity_certified_key(chain, key)?,
    )));

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
    let resolver = Arc::new(SniCertResolver {
        hosts: map,
        fallback,
    });

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

/// A `ServerCertVerifier` that performs the normal rustls/webpki structural
/// chain validation (via a delegate `WebPkiServerVerifier`), and — when it
/// succeeds — additionally runs real OCSP revocation checking
/// (`x509_manager::check_ocsp`) against every certificate in the presented
/// chain except the trust anchor.
///
/// This exists because the native `HttpURLConnection` client path
/// (`http_url_connection.rs::perform`) builds its own rustls `ClientConfig`
/// directly and never calls back into Java's
/// `X509TrustManager.checkServerTrusted` — so it never reaches
/// `x509_manager::validate_chain`, the ONLY place revocation checking was
/// otherwise wired up. Without this verifier, a
/// `PKIXRevocationChecker`-configured `TrustManagerFactory` used only for
/// `HttpsURLConnection.setDefaultSSLSocketFactory` would silently accept a
/// revoked server certificate — exactly the fail-open gap this feature
/// fixes. See
/// `tls-ocsp-clientcert-validation-not-enforced-FIXED.md`.
#[derive(Debug)]
struct OcspAwareServerCertVerifier {
    inner: Arc<dyn rustls::client::danger::ServerCertVerifier>,
    revocation: crate::x509_manager::RevocationConfig,
}

/// A cryptographic-only verifier used when the caller supplied Java
/// `TrustManager`s. Chain and hostname policy is then applied immediately
/// after the handshake by `run_client_trust_check_for_chain`; this preserves
/// custom managers such as Tomcat's test-only `TrustAllCerts` without making
/// their policy invisible to the native HttpURLConnection path.
#[derive(Debug)]
struct PassthroughServerCertVerifier {
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
    /// `(algorithm, host)` when this client engine must perform RFC 2818 /
    /// RFC 6125 endpoint identification, i.e. the application called
    /// `SSLParameters.setEndpointIdentificationAlgorithm("HTTPS"|"LDAPS")`.
    ///
    /// **Why it is HERE and not only in the post-handshake gate.** The
    /// TrustManager consultation is post-handshake TODAY, but not because it
    /// has to be: `JavaKeyManagerResolver::resolve` already runs a Java upcall
    /// from inside `process_new_packets`, reborrowing the caller's context
    /// through `with_active_native_context`. What actually keeps the
    /// TrustManager out of here is that `do_unwrap` holds
    /// `engine_registry()`'s (non-reentrant) write lock across the record
    /// loop, and an `X509ExtendedTrustManager` handed the `SSLEngine` may call
    /// straight back into an engine native. Deferring it costs the property
    /// below in the other direction — see
    /// `testHandshakeFailureOnlyFireExceptionOnce` in
    /// the openssl-key-material-and-engine-residuals write-up (now retired),
    /// where the client sends its `Finished` for a chain its own TrustManager
    /// rejected. The
    /// identity check is not: it is a pure comparison of the presented chain
    /// against the host this side dialled, so it belongs at the point JSSE
    /// makes it, which is *before the client sends its Finished*.
    ///
    /// The difference is visible to the SERVER. netty's
    /// `testClientHostnameValidationFail` asserts BOTH sides fail: with the
    /// check deferred, the client's `Finished` had already gone out, the
    /// server had completed its handshake and netty fired
    /// `SslHandshakeCompletionEvent.SUCCESS` on it — so the test's server
    /// handler recorded `IllegalStateException("handshake complete. expected
    /// failure")` even though the client did reject the certificate.
    endpoint_identity: Option<(String, String)>,
    /// The `ctx_trust_managers_table` key this config's `SSLContext` registered
    /// its Java `TrustManager`s under, so `verify_server_cert` can consult them
    /// AT VERIFICATION TIME rather than after the handshake.
    ///
    /// Per-ENGINE, because `engine_begin` builds one `ClientConfig` per engine
    /// (it already threads `endpoint_identity` — per-engine `SSLParameters`
    /// state — through here for the same reason).
    ///
    /// `None` means "no application TrustManager on this context", in which case
    /// rustls's own chain verification is the whole check and this verifier stays
    /// the pass-through it was named for. The engine's ID and its Java object are
    /// NOT fields: they come from `active_engine_binding()`, published only for
    /// the window in which a call can actually reach Java, and an `ObjectRef`
    /// stored across `engine_begin` would not be GC-stable anyway.
    trust_ctx_key: Option<u64>,
}

impl rustls::client::danger::ServerCertVerifier for PassthroughServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Some((alg, host)) = self.endpoint_identity.as_ref() {
            let mut chain: Vec<Vec<u8>> = Vec::with_capacity(1 + intermediates.len());
            chain.push(end_entity.as_ref().to_vec());
            chain.extend(intermediates.iter().map(|c| c.as_ref().to_vec()));
            if let Err(e) = crate::x509_manager::check_endpoint_identity(&chain, host) {
                let detail =
                    format!("endpoint identification ({alg}) failed for host {host:?}: {e}");
                if crate::nbflags().dbg_tls_auth_ok {
                    eprintln!("[dbg-tls-auth] (in-handshake) {detail}");
                }
                set_last_trust_rejection_detail(&detail);
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidForNameContext {
                        expected: _server_name.to_owned(),
                        presented: Vec::new(),
                    },
                ));
            }
        }
        // Now the application's Java `TrustManager`s, IN the handshake.
        //
        // Deferring this until `!conn.is_handshaking()` — which is what
        // `engine_take_pending_trust_check` does, and all this engine used to do
        // — is by construction after the client has sent its `Finished`. The
        // server therefore completes a valid TLS 1.3 handshake, netty runs
        // `setHandshakeSuccess()`, and the alert that follows cannot fail an
        // already-completed promise: `testHandshakeFailureOnlyFireExceptionOnce`
        // (`SslHandlerTest:1546`) asserts the SERVER's future fails and it did
        // not. Answering `Err` from here makes rustls abort BEFORE `Finished`
        // and emit its fatal alert under handshake keys, which the peer can
        // decrypt — the property
        // `a_verifier_time_rejection_reaches_the_server_while_it_is_still_handshaking`
        // pins.
        //
        // Everything needed to get to Java is already published for this window
        // by `do_unwrap`: `ctx` (the mechanism `JavaKeyManagerResolver::resolve`
        // has used from inside this same `process_new_packets` all along) and the
        // engine binding. When either is absent this is not a path that can reach
        // Java — a native client socket, `HttpURLConnection`, an in-tree
        // `EngineState` test — and the post-handshake gate remains the whole
        // check, exactly as before.
        let Some(trust_ctx_key) = self.trust_ctx_key else {
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        };
        let mut chain: Vec<Vec<u8>> = Vec::with_capacity(1 + intermediates.len());
        chain.push(end_entity.as_ref().to_vec());
        chain.extend(intermediates.iter().map(|c| c.as_ref().to_vec()));
        // One reborrow for the binding read AND the call: the engine reference
        // comes back through its pin, so it must be read with the same ctx that
        // is about to run the upcall.
        let mut engine_id = 0i32;
        let verdict = with_active_native_context(|ctx| {
            let (id, engine_obj) = active_engine_binding(ctx)?;
            engine_id = id;
            let pending = PendingTrustCheck {
                engine_id: id,
                is_client: true,
                peer_chain_der: chain,
                trust_ctx_key: Some(trust_ctx_key),
                // The suite is not settled at verification time, and `auth_type`
                // is only ever a hint a manager may branch or log on — never a
                // security check. `engine_consult_trust_managers` falls back to
                // "RSA", which is what it already does for an unrecognised suite.
                negotiated_cipher_suite_name: None,
                // Already applied above, on the chain rustls handed us.
                endpoint_identity: None,
            };
            Some(engine_consult_trust_managers(
                ctx,
                pending,
                Some(engine_obj),
                TrustCheckMode::InVerifier,
            ))
        })
        .flatten();
        match verdict {
            // No ctx published: not a Java-reachable path (see above).
            None => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            Some(Ok(TrustOutcome::Accepted)) => {
                mark_trust_check_done(engine_id);
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            Some(Ok(TrustOutcome::Rejected(detail, alert))) => {
                mark_trust_check_done(engine_id);
                if crate::nbflags().dbg_tls_auth_ok {
                    eprintln!(
                        "[dbg-tls-auth] (in-handshake) TrustManager rejected: {detail} \
                         -> alert {:?}",
                        alert.alert_description()
                    );
                }
                // NOT `ApplicationVerificationFailure`, which rustls maps to
                // `access_denied` — a POLICY refusal, not a certificate one.
                // That is what this path sent for every rejection, and it cost
                // `SslErrorTest` 12 of its 72 tests where HotSpot passes all
                // 72. `crate::tls_cert_alert` transcribes JSSE's own
                // `CertificateMessage.getCertificateAlert`.
                Err(rustls::Error::InvalidCertificate(
                    alert.certificate_error(detail),
                ))
            }
            // A Java `Error` (not `Exception`) came out of the manager. JSSE lets
            // those through untouched rather than treating them as a rejection,
            // and there is no way to carry one out of a rustls verifier — so do
            // NOT mark the check done, and let the post-handshake gate raise it
            // exactly as it does today. This keeps the `Error`-is-not-a-rejection
            // rule in one place (`throwable_is_error`).
            Some(Err(_)) => Ok(rustls::client::danger::ServerCertVerified::assertion()),
        }
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

impl rustls::client::danger::ServerCertVerifier for OcspAwareServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Structural validation first (chain-to-root, expiry, hostname) —
        // never weaken this: OCSP checking only runs on an already-trusted
        // chain, same ordering `x509_manager::validate_chain` uses (revocation
        // is its "Step 7", after signature verification).
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;

        let mut chain_der: Vec<Vec<u8>> = Vec::with_capacity(1 + intermediates.len());
        chain_der.push(end_entity.as_ref().to_vec());
        chain_der.extend(intermediates.iter().map(|c| c.as_ref().to_vec()));

        let parsed: Vec<crate::x509_manager::ParsedCert> = match chain_der
            .iter()
            .map(|d| crate::x509_manager::parse_certificate(d))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(p) => p,
            // Structural validation above already accepted this chain via a
            // different (webpki) parser; if our own DER walker can't parse
            // it, fail closed rather than silently skip revocation checking.
            Err(e) => {
                return Err(rustls::Error::General(format!(
                    "OCSP revocation check: could not parse presented chain: {e}"
                )));
            }
        };

        // The anchor is whichever root in `inner`'s store actually issued the
        // last presented cert; we don't have that identity directly from
        // `ServerCertVerifier`'s contract, so reuse the last presented cert's
        // own issuer as the "anchor-equivalent" identity for CertID purposes
        // when the chain doesn't include the root itself (the common case —
        // servers don't usually ship their own trust anchor). When the chain
        // DOES end in a self-signed cert, that cert is its own issuer, so
        // `parsed.last()` already carries the right subject/SPKI either way.
        let anchor_like = crate::x509_manager::AnchorInfo {
            subject_der: parsed
                .last()
                .map(|c| c.issuer_der.clone())
                .unwrap_or_default(),
            spki_der: parsed
                .last()
                .map(|c| c.spki_der.clone())
                .unwrap_or_default(),
            full_cert_der: None,
        };
        // The last presented cert's issuer is outside `parsed` (it wasn't
        // shipped) unless the chain is self-signed, matching
        // `validate_chain`'s `last_is_anchor` semantics.
        let last_is_self_signed = parsed
            .last()
            .map(|c| c.issuer_der == c.subject_der)
            .unwrap_or(false);

        if let Err(e) = crate::x509_manager::check_revocation_for_verifier(
            &parsed,
            &anchor_like,
            last_is_self_signed,
            &self.revocation,
        ) {
            return Err(rustls::Error::General(format!(
                "OCSP revocation check failed: {e}"
            )));
        }

        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// As `build_client_config`, but installs `OcspAwareServerCertVerifier` when
/// `revocation` is `Some` — real OCSP checking for the native
/// `HttpURLConnection` client path, which (unlike the `SSLEngine`/
/// `SSLSocketFactory` path) never invokes Java's `checkServerTrusted` and so
/// never reaches `x509_manager::validate_chain` on its own.
pub(crate) fn build_client_config_with_revocation(
    roots: RootCertStore,
    alpn_protocols: &[&str],
    client_auth: Option<(&str, &str)>,
    revocation: Option<crate::x509_manager::RevocationConfig>,
    use_java_trust_manager: bool,
) -> Result<Arc<ClientConfig>, String> {
    if revocation.is_none() && !use_java_trust_manager {
        return build_client_config(roots, alpn_protocols, client_auth);
    }
    build_client_config_ex(
        roots,
        alpn_protocols,
        ClientAuthMode::Fixed(client_auth),
        revocation,
        use_java_trust_manager,
    )
}

/// As `build_client_config_with_revocation`, but plugs in a custom
/// `ResolvesClientCert` instead of a fixed (cert_pem, key_pem) pair — used
/// when a real Java `KeyManager` is available to consult (see
/// `JavaKeyManagerResolver`). Combines both independently-motivated features
/// (server-side OCSP revocation checking and client-side `chooseClientAlias`
/// consultation) since both apply to the same native `HttpURLConnection`
/// client path and a caller may need either, both, or neither.
fn build_client_config_with_revocation_and_resolver(
    roots: RootCertStore,
    alpn_protocols: &[&str],
    revocation: Option<crate::x509_manager::RevocationConfig>,
    resolver: Arc<dyn ResolvesClientCert>,
    use_java_trust_manager: bool,
) -> Result<Arc<ClientConfig>, String> {
    build_client_config_ex(
        roots,
        alpn_protocols,
        ClientAuthMode::Resolver(resolver),
        revocation,
        use_java_trust_manager,
    )
}

/// How `build_client_config_ex` should authenticate this client to the peer.
enum ClientAuthMode<'a> {
    /// A fixed (cert_pem, key_pem) pair, or no client certificate at all —
    /// the pre-`chooseClientAlias` behavior.
    Fixed(Option<(&'a str, &'a str)>),
    /// Consult a real Java `KeyManager` synchronously during the handshake
    /// (see `JavaKeyManagerResolver`).
    Resolver(Arc<dyn ResolvesClientCert>),
}

/// Shared builder for `build_client_config`/`build_client_config_with_revocation`/
/// `build_client_config_with_revocation_and_resolver`: installs
/// `OcspAwareServerCertVerifier` when `revocation` is `Some` (else the plain
/// `WebPkiServerVerifier` via `with_root_certificates`), then applies
/// `client_auth` (a fixed identity, no client auth, or a live resolver).
fn build_client_config_ex(
    roots: RootCertStore,
    alpn_protocols: &[&str],
    client_auth: ClientAuthMode<'_>,
    revocation: Option<crate::x509_manager::RevocationConfig>,
    use_java_trust_manager: bool,
) -> Result<Arc<ClientConfig>, String> {
    build_client_config_ex_with_provider(
        roots,
        alpn_protocols,
        client_auth,
        revocation,
        use_java_trust_manager,
        Arc::new(cbc_augmented_default_provider()),
        &[],
        None,
        None,
    )
}

/// Provider-aware implementation of `build_client_config_ex`.  A distinct
/// provider is required when Java `SSLParameters` narrows the allowed cipher
/// suites, including for the custom-verifier and Java-KeyManager branches.
///
/// `versions` narrows the offered TLS protocol versions (see
/// `protocol_versions_for`); an EMPTY slice means "rustls safe defaults"
/// (TLS 1.3 + TLS 1.2), which is what every caller that has no explicit
/// `SSLSocket.setEnabledProtocols`/`SSLEngine.setEnabledProtocols`
/// restriction passes.
fn build_client_config_ex_with_provider(
    roots: RootCertStore,
    alpn_protocols: &[&str],
    client_auth: ClientAuthMode<'_>,
    revocation: Option<crate::x509_manager::RevocationConfig>,
    use_java_trust_manager: bool,
    provider: Arc<rustls::crypto::CryptoProvider>,
    versions: &[&'static rustls::SupportedProtocolVersion],
    endpoint_identity: Option<(String, String)>,
    // `trust_ctx_key`: the engine's `ctx_trust_managers_table` key, when this
    // config is being built FOR an engine. `None` from the wrapper builders,
    // which serve paths with no engine — their pass-through verifier keeps
    // consulting nothing and the post-handshake gate keeps doing the work,
    // exactly as before.
    trust_ctx_key: Option<u64>,
) -> Result<Arc<ClientConfig>, String> {
    // `with_protocol_versions(&[])` is an error in rustls, and so is a list
    // whose versions the provider cannot serve — fall back to the safe
    // defaults rather than failing the whole connection, matching
    // `cipher_provider_for`'s "an unmappable restriction never starves the
    // connection to zero" contract.
    macro_rules! with_versions {
        ($b:expr) => {{
            let b = $b;
            if versions.is_empty() {
                b.with_safe_default_protocol_versions()
                    .map_err(|e| format!("with_safe_default_protocol_versions failed: {e}"))?
            } else {
                match b.with_protocol_versions(versions) {
                    Ok(v) => v,
                    Err(e) => return Err(format!("with_protocol_versions failed: {e}")),
                }
            }
        }};
    }
    let builder = if use_java_trust_manager {
        let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> =
            Arc::new(PassthroughServerCertVerifier {
                algorithms: provider.signature_verification_algorithms.clone(),
                endpoint_identity,
                trust_ctx_key,
            });
        with_versions!(ClientConfig::builder_with_provider(provider.clone()))
            .dangerous()
            .with_custom_certificate_verifier(verifier)
    } else {
        match revocation {
            Some(revocation) => {
                let roots = Arc::new(roots);
                let inner = rustls::client::WebPkiServerVerifier::builder(roots)
                    .build()
                    .map_err(|e| format!("WebPkiServerVerifier::builder failed: {e}"))?;
                let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> =
                    Arc::new(OcspAwareServerCertVerifier { inner, revocation });
                with_versions!(ClientConfig::builder_with_provider(provider.clone()))
                    .dangerous()
                    .with_custom_certificate_verifier(verifier)
            }
            None => with_versions!(ClientConfig::builder_with_provider(provider))
                .with_root_certificates(roots),
        }
    };
    let mut config = match client_auth {
        ClientAuthMode::Resolver(resolver) => builder.with_client_cert_resolver(resolver),
        ClientAuthMode::Fixed(Some((cert_pem, key_pem))) => {
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
        ClientAuthMode::Fixed(None) => builder.with_no_client_auth(),
    };
    config.alpn_protocols = alpn_protocols
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    Ok(Arc::new(config))
}

// -----------------------------------------------------------------------------
// Synchronous mid-handshake client-certificate selection
// -----------------------------------------------------------------------------
//
// `JavaKeyManagerResolver` is the fix for the "always presents one fixed
// identity" gap: `rustls::client::ResolvesClientCert::resolve` is called BY
// rustls, synchronously, while parsing the server's `CertificateRequest` —
// there is no async/callback-later mechanism, so if the answer depends on
// consulting the real Java `KeyManager.chooseClientAlias` (which it must,
// for a test-installed wrapper like Tomcat's `TrackingKeyManager` to see the
// call and record its side effects, and for a genuinely custom KeyManager to
// have any say at all), that Java call has to happen INSIDE `resolve()`.
//
// Two safety properties this design relies on:
//
//  1. **GC-rootedness without holding `ObjectRef`s long-term.** The resolver
//     itself only stores a plain `u64` key (`km_ctx_key`) — never the
//     `KeyManager` `ObjectRef`s. It is built once per `ClientConfig` and may
//     be cached/reused across many connections and — since rustls requires
//     `Send + Sync` — potentially observed from a different thread than the
//     one that built it. The live `ObjectRef`s are looked up fresh from
//     `ctx_key_managers_table` (GC-rooted, see its doc) every time `resolve`
//     actually runs, exactly mirroring why `EngineState` stores only
//     `trust_managers_ctx_key`, never the `TrustManager`s themselves.
//
//  2. **A valid `&mut dyn NativeContext` at the exact moment `resolve` needs
//     one, despite rustls's fixed trait signature having no room to pass
//     one in.** The caller that drives the handshake (`http_url_connection::
//     perform`) already holds `ctx` for its entire native-call frame; it
//     stashes a raw pointer to it in a thread-local
//     (`set_active_native_context`) for the duration of the
//     `process_new_packets()` call that might invoke `resolve` — and ONLY
//     that duration, cleared via an RAII guard even on an early return/error
//     — then calls `with_active_native_context` to reborrow it. This is sound
//     because rustls calls `resolve` synchronously, on the same thread, from
//     within that exact call; it is never deferred, queued, or handed to
//     another thread. Critically, the caller must NOT be inside a
//     `begin_blocking_region()`/`end_blocking_region()` window when it does
//     this: a blocking region tells the collector this thread is parked and
//     safe to ignore, and `resolve()` here allocates Java objects and runs
//     bytecode (`invoke_virtual`) — running that while "parked" would let a
//     concurrent GC and this thread's own heap mutation race. `perform`
//     ends its blocking region before entering the active-context window and
//     re-enters it immediately after, for exactly this reason.
thread_local! {
    // `'static` here is a lie load-bearing on `ActiveNativeContextGuard`'s
    // `Drop` clearing this before the real, shorter-lived borrow it was
    // erased from could expire — see `set_active_native_context`.
    static ACTIVE_TLS_NATIVE_CTX: std::cell::Cell<Option<*mut (dyn NativeContext + 'static)>> =
        std::cell::Cell::new(None);
}

thread_local! {
    /// The Java `SSLEngine` whose `unwrap` is currently running on this thread,
    /// published alongside the ctx for the duration of `do_unwrap`'s record loop.
    ///
    /// `PassthroughServerCertVerifier` needs it for the THREE-argument
    /// `X509ExtendedTrustManager.checkServerTrusted(chain, authType, SSLEngine)`
    /// overload — the one JSSE uses when the manager is extended, and the one
    /// netty's own wrappers expect. It cannot be a field on the verifier: the
    /// verifier is built once at `engine_begin` and an `ObjectRef` is not
    /// GC-stable across the calls in between, whereas this window is a single
    /// native call.
    /// `(engine id, pin handle, the ObjectRef as it was when pinned)`.
    ///
    /// A pin handle, NOT a bare `ObjectRef`: this is read back from inside
    /// `process_new_packets`, which runs the application's Java `TrustManager`
    /// and therefore allocates, and a moving young collection in that window
    /// relocates the engine mirror. Holding the raw reference across it is the
    /// "native local held live across an allocation" family — the same shape
    /// `engine_run_trust_check`'s own chain-array pin exists for. The `ObjectRef`
    /// is kept alongside only as `read_native_pin`'s fallback.
    static ACTIVE_TLS_ENGINE: std::cell::Cell<Option<(i32, usize, ObjectRef)>> =
        const { std::cell::Cell::new(None) };
}

/// RAII: publish `engine` as the engine currently unwrapping on this thread.
pub(crate) struct ActiveEngineObjGuard {
    _private: (),
}

impl Drop for ActiveEngineObjGuard {
    fn drop(&mut self) {
        ACTIVE_TLS_ENGINE.with(|c| c.set(None));
    }
}

/// Publish `engine` for the record-loop window, PINNED.
///
/// The returned guard clears the thread-local; the pin frame itself is released
/// by the caller's `unpin_native_roots`, which must bracket the same window (a
/// pin taken here and never released would root the engine mirror forever).
fn set_active_engine_binding(
    ctx: &mut dyn NativeContext,
    id: i32,
    engine: ObjectRef,
) -> (ActiveEngineObjGuard, usize) {
    let pin = ctx.pin_native_root(engine);
    ACTIVE_TLS_ENGINE.with(|c| c.set(Some((id, pin, engine))));
    (ActiveEngineObjGuard { _private: () }, pin)
}

/// The engine currently unwrapping on this thread, re-read through its pin so a
/// collection during the Java upcall cannot hand back a stale reference.
fn active_engine_binding(ctx: &mut dyn NativeContext) -> Option<(i32, ObjectRef)> {
    let (id, pin, orig) = ACTIVE_TLS_ENGINE.with(|c| c.get())?;
    Some((id, ctx.read_native_pin(pin, orig)))
}

/// Record that this engine's `TrustManager`s have already been consulted, so
/// `engine_take_pending_trust_check` does not ask them a SECOND time after the
/// handshake. A double consultation is observable — an application manager may
/// count its calls, and netty's test managers do — and the second one would be
/// asking a manager that has already answered.
///
/// Called from inside `verify_server_cert`, i.e. from inside
/// `process_new_packets`, which is only possible because `do_unwrap` runs its
/// record loop with the registry lock DROPPED (see `ConnCheckout`). Before that
/// change this very call would have deadlocked, which is the reason the trust
/// check was deferred in the first place.
fn mark_trust_check_done(id: i32) {
    with_engine(id, |s| s.trust_check_done = true);
}

/// RAII guard returned by `set_active_native_context`; clears the
/// thread-local on drop (including on an early return/`?` inside the
/// handshake loop), so the raw pointer never outlives the native call frame
/// that created it.
pub(crate) struct ActiveNativeContextGuard {
    /// What was published when this guard was created; restored on drop.
    ///
    /// SAVE/RESTORE, not clear-to-`None`, since 2026-08-22. Clearing is correct
    /// for exactly one publisher and silently wrong for two: an inner
    /// `set_active_native_context` would take the window away from the OUTER
    /// frame when it returned, and every later `with_active_native_context`
    /// there would degrade to `None` — which for
    /// `JavaKeyManagerResolver::resolve` means "no client certificate", a
    /// silent wrong answer rather than a crash.
    ///
    /// Nothing nested until `do_check_trusted` started publishing (see that
    /// call site). Making the guard re-entrant is what allows a second
    /// publisher to exist at all, and it costs one word.
    ///
    /// Restoring is sound for the same reason publishing is: the outer pointer
    /// came from a frame that is still live — this guard is nested inside it —
    /// so it cannot have expired while this guard was alive.
    prev: Option<*mut (dyn NativeContext + 'static)>,
}

impl Drop for ActiveNativeContextGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        ACTIVE_TLS_NATIVE_CTX.with(|c| c.set(prev));
    }
}

/// Publish `ctx` for `JavaKeyManagerResolver::resolve` (running on this same
/// thread, synchronously, somewhere inside the caller's next
/// `process_new_packets()`/handshake-loop call) to reborrow. Callers MUST
/// NOT be inside a `begin_blocking_region()` window — see the module doc
/// above. Drop the returned guard (or let it go out of scope) before this
/// call's `ctx` reference itself would become invalid.
pub(crate) fn set_active_native_context(ctx: &mut dyn NativeContext) -> ActiveNativeContextGuard {
    let ptr: *mut dyn NativeContext = ctx;
    // SAFETY: erasing the borrow's lifetime to `'static` here is sound only
    // because `ActiveNativeContextGuard::drop` unconditionally clears this
    // thread-local (even on an early `?` return / panic-driven unwind)
    // before the real `ctx` borrow this pointer came from could expire —
    // see the guard's doc and `with_active_native_context`'s SAFETY note.
    let ptr: *mut (dyn NativeContext + 'static) = unsafe { std::mem::transmute(ptr) };
    let prev = ACTIVE_TLS_NATIVE_CTX.with(|c| c.replace(Some(ptr)));
    ActiveNativeContextGuard { prev }
}

/// Reborrow the `ctx` published by `set_active_native_context`, if any is
/// currently active on this thread. Returns `None` (rather than panicking)
/// when called outside that window, so a resolver invoked in an unexpected
/// context degrades to "no client certificate" instead of crashing.
fn with_active_native_context<R>(f: impl FnOnce(&mut dyn NativeContext) -> R) -> Option<R> {
    let ptr = ACTIVE_TLS_NATIVE_CTX.with(|c| c.get())?;
    // SAFETY: `ptr` was published by `set_active_native_context` from a
    // `&mut dyn NativeContext` that is still borrowed for the entire
    // enclosing native call (`http_url_connection::perform`'s handshake
    // loop). We are executing synchronously, on the same thread, inside a
    // call reachable only from within that same call frame
    // (`rustls`'s `process_new_packets` -> `resolve`), so the pointer is
    // still valid. The `ActiveNativeContextGuard` clears the thread-local
    // (via `Drop`, so even an early `?` return runs it) before that frame
    // returns, so this pointer can never be read after it would dangle.
    Some(f(unsafe { &mut *ptr }))
}

/// RAII: mark this thread GC-blocked for ONE blocking socket syscall, through
/// the native context published by [`set_active_native_context`].
///
/// The TLS request path cannot open a blocking region around the whole
/// exchange the way the plain-HTTP path does, because it has to run Java
/// (`JavaKeyManagerResolver::resolve` -> `chooseClientAlias`) in the middle of
/// the handshake, and a GC-blocked thread must not be executing bytecode. Its
/// answer used to be to open no region at all — so an HTTPS request sat in
/// `connect`/`recv`/`send` for up to the request timeout while the collector
/// still counted it as a cooperative mutator, and a stop-the-world pause that
/// began during a request waited for a thread that could not reach a
/// safepoint. Under `CRATONVM_DBG_GC_STRESS=1048576` that wedges every time:
/// `STW cross-thread JIT takeover is still waiting for cooperative mutators`,
/// repeating forever, with `--nojit` too.
///
/// The split that resolves it: rustls does its socket I/O in `read_tls` /
/// `write_tls` / `complete_io` and its protocol work — including every Java
/// upcall — in `process_new_packets`, which touches no socket. So the region
/// belongs around the SYSCALL, not around the exchange. Wrapping the socket
/// (see `net_phase_e::GcBlockingSocket`) puts it there and nowhere else: no
/// layer above can accidentally hold it across an upcall, the same reason
/// `EintrIo` wraps the socket rather than patching each call site.
///
/// Returns an inert guard when no context is published — that is the
/// pre-existing behaviour, not a new failure mode, and the paths this is used
/// from all publish one.
pub(crate) struct GcBlockedSyscall {
    entered: bool,
}

/// Open a [`GcBlockedSyscall`] region for the duration of the returned guard.
pub(crate) fn gc_blocked_syscall() -> GcBlockedSyscall {
    // The reborrow ends before the caller's syscall runs, so this never holds
    // a `&mut dyn NativeContext` across the blocking call — nor across the
    // `resolve` upcall, which reborrows it again from `process_new_packets`.
    let entered = with_active_native_context(|ctx| ctx.begin_blocking_region()).is_some();
    GcBlockedSyscall { entered }
}

impl Drop for GcBlockedSyscall {
    fn drop(&mut self) {
        if self.entered {
            // Symmetric on every path, including an `Err(...)?` out of the
            // syscall and an unwind: an unbalanced enter leaves this thread
            // counted as blocked forever, which is the same hang read from
            // the other side.
            let _ = with_active_native_context(|ctx| ctx.end_blocking_region());
        }
    }
}

/// Map the `SignatureScheme`s a server's `CertificateRequest` advertises to
/// the JSSE-style `keyType` strings (`"RSA"`, `"EC"`, …)
/// `X509KeyManager.chooseClientAlias`/`getClientAliases` expect. Order is
/// stable and deduplicated; falls back to `["RSA"]` when nothing recognized
/// is offered (matches this module's other RSA-favoring defaults) so a
/// server that (unusually) advertises no schemes still gets a `keyType` hint
/// rather than an empty array.
fn key_types_from_sigschemes(schemes: &[SignatureScheme]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str, out: &mut Vec<String>| {
        if !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    };
    for scheme in schemes {
        match scheme {
            SignatureScheme::RSA_PKCS1_SHA1
            | SignatureScheme::RSA_PKCS1_SHA256
            | SignatureScheme::RSA_PKCS1_SHA384
            | SignatureScheme::RSA_PKCS1_SHA512
            | SignatureScheme::RSA_PSS_SHA256
            | SignatureScheme::RSA_PSS_SHA384
            | SignatureScheme::RSA_PSS_SHA512 => push("RSA", &mut out),
            SignatureScheme::ECDSA_SHA1_Legacy
            | SignatureScheme::ECDSA_NISTP256_SHA256
            | SignatureScheme::ECDSA_NISTP384_SHA384
            | SignatureScheme::ECDSA_NISTP521_SHA512 => push("EC", &mut out),
            SignatureScheme::ED25519 => push("Ed25519", &mut out),
            SignatureScheme::ED448 => push("Ed448", &mut out),
            _ => {}
        }
    }
    if out.is_empty() {
        out.push("RSA".to_string());
    }
    out
}

/// Build a Java `Principal[]` from the server's acceptable-issuer DER hints,
/// for the `issuers` parameter of `chooseClientAlias`/`getClientAliases`.
/// Each hint is rendered to an RFC 4514 DN string (via
/// `security_manager::x509::parse_name_dn`) and wrapped in a synthetic
/// `X500Principal`, mirroring exactly how `phases_late.rs`'s
/// `X509Certificate.getSubjectX500Principal()` already builds one (same
/// class, same 1-field/string convention) — so `Principal.getName()` on the
/// result behaves the same way existing, already-exercised code produces.
/// Hints that fail to parse (malformed DER) are skipped rather than aborting
/// the whole call — a partial issuer list is still useful context for
/// `chooseClientAlias`, and none at all is a valid "send whatever you have"
/// signal per the trait's own contract.
/// GC NOTE: `arr` and each `princ` outlive allocating calls (`create_string`,
/// `alloc_concurrent_synthetic`), so both are rooted in a handle scope and
/// re-read. Same defect class as `attach_trust_managers_to_ctx`'s 2026-08-01
/// fix — see the `SSLContext.init` comment in `net_phase_e.rs` for the
/// measurement that showed a stale copy silently produces an anonymous client.
/// The caller must root the returned array itself before allocating again.
fn build_issuer_principals(
    ctx: &mut dyn NativeContext,
    root_hint_subjects: &[&[u8]],
) -> Result<ObjectRef, MethodCallFailed> {
    let dn_strings: Vec<String> = root_hint_subjects
        .iter()
        .filter_map(|der| crate::security_manager::x509::parse_name_dn(der).ok())
        .collect();
    let cls_id = ctx
        .ensure_class_initialized("java/security/Principal")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(cls_id, dn_strings.len());
    let arr_h = scope.root(arr);
    for (i, dn) in dn_strings.iter().enumerate() {
        let princ = try_alloc_concurrent_synthetic(
            &mut *scope,
            "javax/security/auth/x500/X500Principal",
            1,
        )?;
        let princ_h = scope.root(princ);
        let s = scope.create_string(dn);
        let princ = scope.get(&princ_h);
        scope.set_field(princ, 0, Value::Object(Some(s)));
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(princ)));
    }
    Ok(scope.get(&arr_h))
}

/// GC NOTE: see [`build_issuer_principals`] — `arr` is rooted because
/// `create_string` below can move it.
fn materialize_java_string_array(ctx: &mut dyn NativeContext, items: &[String]) -> ObjectRef {
    let cls_id = ctx
        .ensure_class_initialized("java/lang/String")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(cls_id, items.len());
    let arr_h = scope.root(arr);
    for (i, s) in items.iter().enumerate() {
        let js = scope.create_string(s);
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(js)));
    }
    scope.get(&arr_h)
}

/// True if `result` is an `Err(ExceptionThrown)` wrapping a real
/// `java.lang.AbstractMethodError`. See `resolve_via_java`'s retry-on-first-
/// hit call site for why this specific exception gets a bounded retry
/// instead of being treated as a genuine dispatch failure.
fn is_abstract_method_error(
    ctx: &mut dyn NativeContext,
    result: &Result<Option<Value>, cratonvm_types::error::MethodCallFailed>,
) -> bool {
    match result {
        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc)) => {
            ctx.class_name_arc_of_id(ctx.class_id_of_object(*exc))
                .as_deref()
                == Some("java/lang/AbstractMethodError")
        }
        _ => false,
    }
}

/// Consults the real Java `KeyManager.chooseClientAlias` (and
/// `getPrivateKey`) to pick a client certificate matching the server's
/// `CertificateRequest`, instead of always presenting one fixed identity.
/// See the module doc above this struct for the GC-safety/re-entrancy design
/// this relies on.
#[derive(Debug)]
struct JavaKeyManagerResolver {
    /// Key into `ctx_key_managers_table` — never the `ObjectRef`s themselves.
    km_ctx_key: u64,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl JavaKeyManagerResolver {
    /// Try each configured `KeyManager` in turn (matching real JSSE's
    /// `SSLContextImpl`, which does the same) until one's
    /// `chooseClientAlias` returns a non-null alias with resolvable
    /// cert/key material.
    fn resolve_via_java(
        &self,
        ctx: &mut dyn NativeContext,
        root_hint_subjects: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Result<Option<Arc<CertifiedKey>>, MethodCallFailed> {
        let dbg = crate::nbflags().dbg_tls_auth_ok;
        let mut km_list = match ctx_key_managers_table().lock().get(&self.km_ctx_key) {
            Some(list) => list.clone(),
            None => return Ok(None),
        };
        if dbg {
            eprintln!(
                "[dbg-tls-auth] JavaKeyManagerResolver::resolve km_ctx_key={} km_count={} root_hint_subjects={} sigschemes={:?}",
                self.km_ctx_key,
                km_list.len(),
                root_hint_subjects.len(),
                sigschemes
            );
        }
        if km_list.is_empty() {
            return Ok(None);
        }
        let key_types = key_types_from_sigschemes(sigschemes);
        if dbg {
            eprintln!(
                "[dbg-tls-auth] JavaKeyManagerResolver key_types={:?}",
                key_types
            );
        }
        // GC: both arrays are handed to `chooseClientAlias` on every loop
        // iteration below, and every `invoke_virtual` between here and there
        // can move them. `key_type_arr` in particular is built BEFORE
        // `build_issuer_principals`, which allocates one synthetic principal
        // and one String per issuer hint. A stale `String[] keyType` makes a
        // real `SunX509KeyManagerImpl.chooseClientAlias` return null, which is
        // indistinguishable from "the application declined to present a
        // certificate" — the client then sends an EMPTY Certificate and a
        // server with `clientAuth=NEED` answers `CertificateRequired`. That is
        // the reported failure; see the `SSLContext.init` comment in
        // `net_phase_e.rs` for the measurement.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let key_type_arr = materialize_java_string_array(&mut *scope, &key_types);
        let key_type_h = scope.root(key_type_arr);
        let issuers_arr = build_issuer_principals(&mut *scope, root_hint_subjects);
        let issuers_h = scope.root(issuers_arr?);
        let ctx = &mut scope;

        // Pin every KeyManager ObjectRef before any call that can allocate
        // (invoke_virtual below) — a moving GC triggered by that call could
        // otherwise relocate an entry we haven't gotten to yet. Mirrors the
        // `apps_h2.rs`/`atomic_updater.rs` batch-pin pattern: keep each
        // individual handle (not assumed-sequential arithmetic), unpin the
        // whole batch via the FIRST handle at the end.
        let pins: Vec<usize> = km_list
            .iter()
            .map(|&obj| ctx.pin_native_root(obj))
            .collect();
        let first_pin = pins[0];

        let result = (|| {
            for (i, &pin) in pins.iter().enumerate() {
                km_list[i] = ctx.read_native_pin(pin, km_list[i]);
                let km_obj = km_list[i];
                if dbg {
                    let cls_name = ctx.class_name_of_id(ctx.class_id_of_object(km_obj));
                    eprintln!(
                        "[dbg-tls-auth] JavaKeyManagerResolver km_obj[{}] class={:?}",
                        i, cls_name
                    );
                }
                // FIX (tomcat-clientauth-engine-config): a caller-installed
                // `KeyManager` wrapper (e.g. Tomcat's `TrackingKeyManager`,
                // `test/org/apache/tomcat/util/net/TesterSupport.java`)
                // delegates to whatever `KeyManagerFactory.getKeyManagers()`
                // returned. That was ROOT-CAUSED (not just worked around) —
                // see `phases_late.rs`'s `KeyManagerFactory.getKeyManagers()`
                // handler's own doc comment for the full story: it used to
                // return an object stamped with the bare
                // `javax/net/ssl/X509KeyManager` INTERFACE's own class id,
                // whose 6 methods are all abstract with no Code, so any
                // caller invoking one directly hit `AbstractMethodError`
                // unconditionally and deterministically (confirmed via
                // `CRATONVM_DBG_NOCODE=1` tracing — NOT a timing/vtable-
                // visibility race, an earlier hypothesis this session
                // initially chased and disproved: `ensure_class_initialized`
                // immediately before the call never helped, and a bounded
                // retry-with-backoff (below) failed identically on every
                // attempt, every time — the real receiver class was wrong,
                // not "not yet ready"). Fixed at the source in
                // `getKeyManagers()`. The retry loop below is kept as a
                // narrowly-scoped defensive fallback (harmless: it only
                // fires on this exact exception class, and costs nothing
                // when it never fires) in case some OTHER, genuinely
                // transient cause of the same symptom is hit by a future
                // caller shape this session didn't exercise.
                // Re-read both argument arrays through their handles on every
                // iteration: the previous iteration's `invoke_virtual`s can
                // have moved them.
                let args = [
                    Value::Object(Some(ctx.get(&key_type_h))),
                    Value::Object(Some(ctx.get(&issuers_h))),
                    Value::Object(None),
                ];
                let mut choose_result = ctx.invoke_virtual(
                    km_obj,
                    "chooseClientAlias",
                    "([Ljava/lang/String;[Ljava/security/Principal;Ljava/net/Socket;)Ljava/lang/String;",
                    &args,
                );
                let mut retry_attempt = 0;
                while is_abstract_method_error(&mut **ctx, &choose_result) && retry_attempt < 3 {
                    retry_attempt += 1;
                    std::thread::yield_now();
                    std::thread::sleep(std::time::Duration::from_millis(5 * retry_attempt));
                    km_list[i] = ctx.read_native_pin(pin, km_list[i]);
                    let km_obj = km_list[i];
                    let args = [
                        Value::Object(Some(ctx.get(&key_type_h))),
                        Value::Object(Some(ctx.get(&issuers_h))),
                        Value::Object(None),
                    ];
                    choose_result = ctx.invoke_virtual(
                        km_obj,
                        "chooseClientAlias",
                        "([Ljava/lang/String;[Ljava/security/Principal;Ljava/net/Socket;)Ljava/lang/String;",
                        &args,
                    );
                    if dbg {
                        eprintln!(
                            "[dbg-tls-auth] JavaKeyManagerResolver chooseClientAlias[{}] RETRY#{} after AbstractMethodError -> {:?}",
                            i, retry_attempt, choose_result
                        );
                    }
                }
                let choose_result = choose_result;
                let alias = match &choose_result {
                    Ok(Some(Value::Object(Some(s)))) => ctx.read_string(*s),
                    _ => None,
                };
                if dbg {
                    let exc_cls = match &choose_result {
                        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc)) => {
                            Some(ctx.class_name_of_id(ctx.class_id_of_object(*exc)))
                        }
                        _ => None,
                    };
                    eprintln!(
                        "[dbg-tls-auth] JavaKeyManagerResolver chooseClientAlias[{}] -> {:?} (raw={:?}, exc_class={:?})",
                        i, alias, choose_result, exc_cls
                    );
                }
                km_list[i] = ctx.read_native_pin(pin, km_list[i]);
                let Some(alias) = alias else { continue };

                let pk_args = [Value::Object(Some(ctx.create_string(&alias)))];
                // `create_string` above allocates, so the receiver is re-read
                // AFTER it rather than before — the pre-`create_string` copy
                // taken a few lines up is already potentially stale.
                km_list[i] = ctx.read_native_pin(pin, km_list[i]);
                let pk_result = ctx.invoke_virtual(
                    km_list[i],
                    "getPrivateKey",
                    "(Ljava/lang/String;)Ljava/security/PrivateKey;",
                    &pk_args,
                );
                let pk_obj = match &pk_result {
                    Ok(Some(Value::Object(Some(pk)))) => Some(*pk),
                    _ => None,
                };
                if dbg {
                    eprintln!(
                        "[dbg-tls-auth] JavaKeyManagerResolver getPrivateKey[{}] alias={} -> present={} (raw={:?})",
                        i, alias, pk_obj.is_some(), pk_result
                    );
                }
                km_list[i] = ctx.read_native_pin(pin, km_list[i]);
                let Some(pk_obj) = pk_obj else { continue };

                let km_id = crate::x509_manager::km_id_from_private_key_mirror(&**ctx, pk_obj);
                if dbg {
                    eprintln!("[dbg-tls-auth] JavaKeyManagerResolver km_id_from_private_key_mirror -> {:?}", km_id);
                }
                let Some(km_id) = km_id else {
                    continue;
                };
                let material = crate::x509_manager::km_alias_material(km_id, &alias);
                if dbg {
                    eprintln!(
                        "[dbg-tls-auth] JavaKeyManagerResolver km_alias_material(km_id={}, alias={}) -> chain_len={:?} key_len={:?}",
                        km_id, alias,
                        material.as_ref().map(|(c, _)| c.len()),
                        material.as_ref().map(|(_, k)| k.len())
                    );
                }
                let Some((chain_der, key_der)) = material else {
                    continue;
                };
                let cert_chain: Vec<CertificateDer<'static>> =
                    chain_der.into_iter().map(CertificateDer::from).collect();
                if cert_chain.is_empty() {
                    continue;
                }
                // Repairs the JDK-shaped EC key on the way — without it a
                // JDK-generated EC *client* identity resolves to `Err` here,
                // the client sends an empty Certificate, and the far end
                // answers `CertificateRequired`, a failure that names neither
                // the key nor this decision.
                match certified_key_from_der_repairing_ec(cert_chain, key_der, &self.provider) {
                    Ok(ck) => return Some(Arc::new(ck)),
                    Err(e) => {
                        if dbg {
                            eprintln!("[dbg-tls-auth] JavaKeyManagerResolver CertifiedKey::from_der failed: {e}");
                        }
                    }
                }
            }
            None
        })();

        ctx.unpin_native_roots(first_pin);
        Ok(result)
    }
}

impl ResolvesClientCert for JavaKeyManagerResolver {
    fn resolve(
        &self,
        root_hint_subjects: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        let dbg = crate::nbflags().dbg_tls_auth_ok;
        if dbg {
            eprintln!(
                "[dbg-tls-auth] JavaKeyManagerResolver::resolve CALLED km_ctx_key={} root_hint_subjects={}",
                self.km_ctx_key,
                root_hint_subjects.len()
            );
        }
        // A resolver that HAS key managers but produces nothing makes the
        // client send an empty Certificate. That is legitimate when the
        // application's own `chooseClientAlias` declines, but it is also how
        // every internal failure in this path presents — and the far end then
        // reports something that names neither this VM nor this decision
        // (`received fatal alert: CertificateRequired` from a server with
        // `clientAuth=NEED`, which is what
        // `jdkclienthttprequestfactory-certificaterequired-alert-20260806`
        // was opened on). Say so here, once per handshake, so a recurrence
        // from a cause other than the stale-`ObjectRef` one fixed alongside
        // this is diagnosable from an ordinary run's log.
        let had_key_managers = self.has_certs();
        // `resolve` is rustls' own trait method and returns `Option`: there
        // is nowhere to put a refusal. Absorb it to "no key", which is what
        // rustls already does when a resolver has nothing to offer. The
        // `--jdk-only` violation was recorded when the class was refused.
        let out = with_active_native_context(|ctx| {
            self.resolve_via_java(ctx, root_hint_subjects, sigschemes)
        })
        .and_then(|r| r.ok());
        if dbg && out.is_none() {
            eprintln!("[dbg-tls-auth] JavaKeyManagerResolver::resolve NO active native context");
        }
        let had_active_native_context = out.is_some();
        let resolved = out.flatten();
        if had_key_managers && resolved.is_none() {
            tracing::warn!(
                target: "tls",
                km_ctx_key = self.km_ctx_key,
                acceptable_issuers = root_hint_subjects.len(),
                had_active_native_context,
                "client certificate requested by the peer, but this SSLContext's \
                 KeyManagers produced none — sending an empty certificate. A peer \
                 requiring client auth will answer with a CertificateRequired alert. \
                 Run with CRATONVM_DBG_TLS_AUTH=1 for the per-stage trace"
            );
        }
        resolved
    }

    fn has_certs(&self) -> bool {
        // A pure data check (no Java call) — safe to call before the active
        // native-context window opens (rustls calls this very early, at
        // `ClientConnection::new`/`start_handshake`, before the handshake
        // loop that establishes that window even begins).
        let out = ctx_key_managers_table()
            .lock()
            .get(&self.km_ctx_key)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] JavaKeyManagerResolver::has_certs CALLED km_ctx_key={} -> {}",
                self.km_ctx_key, out
            );
        }
        out
    }
}

/// A `ResolvesClientCert` that records whether the client actually PRESENTED a
/// certificate, and otherwise delegates.
///
/// rustls exposes no "did I send a client certificate" signal on
/// `ClientConnection`, and the difference is observable through JSSE:
/// `SSLSession.getLocalCertificates()` must answer null on a client that had a
/// `KeyManager` configured but was never asked for a certificate (a server
/// running `ClientAuth.NONE` sends no `CertificateRequest`, so the resolver is
/// never consulted). `SSLEngineTest.testSessionLocalWhenNonMutual` sets up
/// exactly that and asserts null; reporting the identity the engine merely HAD
/// available answered `expected: <null> but was: <[[…]]>`.
///
/// `resolve` is the right place because rustls calls it if and only if the
/// server asked, and a `Some` answer is exactly the certificate that then goes
/// on the wire.
#[derive(Debug)]
struct RecordingClientCertResolver {
    inner: Arc<dyn ResolvesClientCert>,
    presented: Arc<std::sync::atomic::AtomicBool>,
}

impl ResolvesClientCert for RecordingClientCertResolver {
    fn resolve(
        &self,
        root_hint_subjects: &[&[u8]],
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let out = self.inner.resolve(root_hint_subjects, sigschemes);
        if out.is_some() {
            self.presented
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        out
    }

    fn has_certs(&self) -> bool {
        self.inner.has_certs()
    }
}

/// The JSSE name for a negotiated rustls `CipherSuite`.
///
/// The inverse of [`java_cipher_name_to_suite`], and it has to exist: rustls's
/// `Debug` spelling of a TLS 1.3 suite carries a `13` infix
/// (`TLS13_AES_128_GCM_SHA256`) that JSSE's name does not
/// (`TLS_AES_128_GCM_SHA256`), and eight call sites were reporting the `Debug`
/// string verbatim as `SSLSession.getCipherSuite()`. netty's
/// `SSLEngineTest.testGetCiphersuite` compares it against the name it asked for
/// and got `expected: <TLS_AES_128_GCM_SHA256> but was:
/// <TLS13_AES_128_GCM_SHA256>`; `assertArrayContains` failed the same way.
///
/// Only the TLS 1.3 triple differs — every TLS 1.2 suite rustls names is
/// already spelled the JSSE way — so this is an explicit list rather than a
/// blind `replace("TLS13_", "TLS_")`, which would also rewrite a future suite
/// whose real name happens to contain that text.
///
/// `_pub` wrapper for `http_url_connection`, whose https branch reported the
/// `Debug` spelling for the same reason the eight sites this function was
/// written for did.
pub(crate) fn suite_to_java_cipher_name_pub(suite: rustls::CipherSuite) -> String {
    suite_to_java_cipher_name(suite)
}

fn suite_to_java_cipher_name(suite: rustls::CipherSuite) -> String {
    use rustls::CipherSuite::*;
    match suite {
        TLS13_AES_128_GCM_SHA256 => "TLS_AES_128_GCM_SHA256".to_string(),
        TLS13_AES_256_GCM_SHA384 => "TLS_AES_256_GCM_SHA384".to_string(),
        TLS13_CHACHA20_POLY1305_SHA256 => "TLS_CHACHA20_POLY1305_SHA256".to_string(),
        // The three arms above are the suites this VM's crypto provider will
        // actually negotiate; the `TLS13_` infix, though, is a property of
        // rustls's ENUM SPELLING and not of those three names — the CCM pair
        // carries it too. Falling through to the prefix rewrite (measured on
        // HotSpot 25.0.3+9-LTS and unit-tested in both directions in
        // `http_url_connection`; see
        // `docs/known-issues/jdk-only/E3-1-the-cipher-name-helper-and-its-real-denominator.md`)
        // keeps ONE translation in the tree rather than two that agree today.
        // It is prefix-only, so a name that is already the registry's is
        // returned untouched.
        other => crate::http_url_connection::jsse_cipher_suite_name(&format!("{other:?}")),
    }
}

/// Map a Java `SSLEngine.setEnabledCipherSuites` name to the matching rustls
/// `CipherSuite`. Only covers the suites this module ever advertises via
/// `getSupportedCipherSuites`/`getEnabledCipherSuites` (see the two identical
/// 13-entry lists elsewhere in this file) — the full negotiable set for the
/// `ring` crypto provider, plus the four TLS1.2 CBC suites added by T-CBC.1
/// (`t27_tls_cbc`, since `ring` itself never implements CBC-mode suites).
/// TLS 1.3 suite names differ (Java drops the "13" infix rustls uses),
/// everything else matches verbatim.
fn java_cipher_name_to_suite(name: &str) -> Option<rustls::CipherSuite> {
    use rustls::CipherSuite::*;
    Some(match name {
        "TLS_AES_128_GCM_SHA256" => TLS13_AES_128_GCM_SHA256,
        "TLS_AES_256_GCM_SHA384" => TLS13_AES_256_GCM_SHA384,
        "TLS_CHACHA20_POLY1305_SHA256" => TLS13_CHACHA20_POLY1305_SHA256,
        "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => {
            TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
        }
        "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => {
            TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256
        }
        "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256" => TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256,
        "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256" => TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256,
        "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384" => TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384,
        "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384" => TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384,
        // FIX (tls-handshake-enforcement-gap, doc 21): classic finite-field
        // DHE suites map onto their ECDHE analogue.
        //
        // rustls implements no finite-field Diffie-Hellman key exchange in
        // any crypto provider (`ring`/`aws-lc-rs` ship ECDHE only) — a real
        // upstream limitation we cannot close from here. Refusing to map
        // them at all, though, is strictly worse than mapping them onto the
        // suite that differs ONLY in the key-exchange group: a caller's
        // cipher POLICY (which certificate/authentication algorithm the peer
        // must use, and which bulk cipher + PRF strength) is fully preserved
        // by this substitution, and it is exactly that policy every affected
        // caller is expressing. Left unmapped, `cipher_provider_for` fell
        // back to the unrestricted provider, so a deliberately incompatible
        // restriction quietly negotiated something else and SUCCEEDED —
        // `TestSSLHostConfigCipher.testTls12CipherNotAvailable` and
        // `TestSSLHostConfigCompat.testHostECwithRSAClient` both expect
        // `SSLHandshakeException` and got a normal response instead.
        //
        // The mapping is applied consistently on BOTH ends (the same
        // function serves the server's `SSLHostConfig.ciphers` and the
        // client's `setEnabledCipherSuites`), so an intersection that is
        // empty in JSSE terms stays empty here and a non-empty one stays
        // non-empty. What is NOT reproduced is the wire-level key exchange:
        // a peer that genuinely only offers FFDHE still cannot be talked to.
        // These names are also advertised from the supported-suite lists
        // (`getSupportedCipherSuites`/`getSupportedSSLParameters`) so
        // Tomcat's `SSLUtilBase.getEnabled` stops silently dropping them
        // from a connector's configured list.
        //
        // Only the `DHE_RSA` family is mapped. `DHE_DSS` is deliberately
        // left unmapped: substituting an RSA-authenticated suite for it
        // would change the AUTHENTICATION algorithm, which is exactly the
        // property this mapping exists to preserve.
        "TLS_DHE_RSA_WITH_AES_128_GCM_SHA256" => TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        "TLS_DHE_RSA_WITH_AES_256_GCM_SHA384" => TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        _ => return None,
    })
}

/// The Java cipher-suite names CratonVM's TLS stack advertises as supported,
/// in JSSE preference order. Single source of truth for
/// `SSLEngine.getSupportedCipherSuites`, `SSLSocket
/// .getSupportedCipherSuites` and `SSLContext.getSupportedSSLParameters()`,
/// which previously each kept their own hand-maintained copy and had already
/// drifted apart.
pub(crate) const SUPPORTED_CIPHER_SUITE_NAMES: &[&str] = &[
    "TLS_AES_128_GCM_SHA256",
    "TLS_AES_256_GCM_SHA384",
    "TLS_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
    // T-CBC.1: real CBC-mode suites, see t27_tls_cbc /
    // rustls-cbc-cipher-suites-not-supported.md
    "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384",
    // Negotiated as their ECDHE analogue — see `java_cipher_name_to_suite`.
    "TLS_DHE_RSA_WITH_AES_128_GCM_SHA256",
    "TLS_DHE_RSA_WITH_AES_256_GCM_SHA384",
];

/// `SSLSession.getPacketBufferSize()` — MEASURED on HotSpot 25.0.3+9 in every
/// state (`G25Probe`): fresh engine, TLS 1.3 with AES-128-GCM, TLS 1.3 with
/// AES-256-GCM. Unlike its neighbour this one really is a constant.
pub(crate) const JSSE_PACKET_BUFFER_SIZE: i32 = 16709;

/// `SSLSession.getApplicationBufferSize()` for a session that has NOT
/// negotiated. MEASURED: 16704, which is [`JSSE_PACKET_BUFFER_SIZE`] minus the
/// 5-byte TLS record header.
pub(crate) const JSSE_APPLICATION_BUFFER_SIZE_FRESH: i32 = 16704;

/// `SSLSession.getApplicationBufferSize()` after a completed handshake.
/// MEASURED: 16676 for TLS 1.3 under both `TLS_AES_128_GCM_SHA256` and
/// `TLS_AES_256_GCM_SHA384` — the value moves when a suite is negotiated but
/// not between suites, which is why one constant per STATE is enough and a
/// per-suite table is not needed. See the accessor's comment for the audit
/// that made raising this from the old 16384 safe.
pub(crate) const JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED: i32 = 16676;

/// `ring`'s default `CryptoProvider`, augmented with the T-CBC.1 CBC-mode
/// TLS1.2 suites (`crate::t27_tls_cbc`) that `ring` itself never implements —
/// see `rustls-cbc-cipher-suites-not-supported.md`.
/// Every call site that used to construct `rustls::crypto::ring::default_provider()`
/// directly now goes through this instead, so the CBC suites are negotiable
/// (not just reported) everywhere TLS connections get set up.
fn cbc_augmented_default_provider() -> rustls::crypto::CryptoProvider {
    let mut provider = rustls::crypto::ring::default_provider();
    provider.cipher_suites.extend([
        rustls::SupportedCipherSuite::from(
            &crate::t27_tls_cbc::TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256,
        ),
        rustls::SupportedCipherSuite::from(
            &crate::t27_tls_cbc::TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256,
        ),
        rustls::SupportedCipherSuite::from(
            &crate::t27_tls_cbc::TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384,
        ),
        rustls::SupportedCipherSuite::from(
            &crate::t27_tls_cbc::TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384,
        ),
    ]);
    provider
}

/// Build a `CryptoProvider` restricted to `enabled` (Java cipher-suite names),
/// falling back to the unrestricted (CBC-augmented) default when `enabled` is
/// empty or maps to nothing we recognize — so an unmappable/empty list can
/// never starve the connection down to zero usable suites (which would make
/// the builder error out instead of just failing to restrict as intended).
fn cipher_provider_for(enabled: &[String]) -> Arc<rustls::crypto::CryptoProvider> {
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
pub(crate) fn is_cipher_suite_name(name: &str) -> bool {
    java_cipher_name_to_suite(name).is_some()
        || SUPPORTED_CIPHER_SUITE_NAMES.contains(&name)
        || name.starts_with("TLS_")
        || name.starts_with("SSL_")
}

/// True if at least one of `ciphers` maps to a real rustls `CipherSuite` (see
/// `java_cipher_name_to_suite`). Callers that would otherwise silently fall
/// back to an unrestricted connection (see `cipher_provider_for`) should use
/// this to detect that case up front and leave an existing connection alone
/// instead of tearing it down for a restriction that cannot actually be
/// enforced through rustls.
pub(crate) fn any_cipher_mappable(ciphers: &[String]) -> bool {
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
fn protocol_versions_for(enabled: &[String]) -> Vec<&'static rustls::SupportedProtocolVersion> {
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
pub(crate) fn protocol_restriction_is_real(protocols: &[String]) -> bool {
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
pub(crate) fn jsse_would_send_sni(host: &str) -> bool {
    if host.is_empty() || !host.contains('.') || host.contains(':') {
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
    true
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
pub(crate) fn client_config_for_host(config: Arc<ClientConfig>, host: &str) -> Arc<ClientConfig> {
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
fn provider_and_versions(
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
struct PassthroughClientCertVerifier {
    mandatory: bool,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
    /// Acceptable-CA list to advertise in the `CertificateRequest` — the
    /// delegating Java `TrustManager`'s `getAcceptedIssuers()`, snapshotted at
    /// `SSLContext.init` time (see `capture_accepted_issuer_dns`). Empty when
    /// no manager was registered or it accepts any issuer.
    root_hints: Vec<rustls::DistinguishedName>,
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
fn build_server_config_single_cert_passthrough_client_auth(
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
pub(crate) fn build_client_config_ciphers(
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
pub(crate) fn rustls_client_connect(
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
pub(crate) const NATIVE_TLS_UNNAMEABLE_SUITE: &str = "UNKNOWN";

/// Why [`rustls_server_accept_within`] did not return a stream.
///
/// **`TimedOut` is not a `Failed("...timed out")`.** `SSLServerSocket.accept()`
/// turns a failure into `java.io.IOException` and an expiry into
/// `java.net.SocketTimeoutException`, and an accept loop distinguishes them by
/// catching the latter and going round again — `RSslLiveSession.serve` is
/// written exactly that way. Collapsing the two into one string error would
/// make an ordinary idle tick look like a dead listener.
pub(crate) enum AcceptFailure {
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
pub(crate) fn rustls_server_accept_within(
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
                // NOT CHANGED, and disclosed rather than fixed: the `"TLSv1.2"`
                // beside the suite is a fabrication of the same species the
                // comment above refuses for the cipher — a real, plausible
                // protocol version asserted about a handshake whose version is
                // equally unknowable here. It is worse than a conservative
                // under-report on THESE arms specifically, because they are the
                // *legacy* acceptors: a caller testing "am I on at least TLS
                // 1.2" is told yes for a connection that may be 1.0 or 1.1.
                // Left alone because changing it is a behaviour change on a
                // live, succeeding connection and this lane cannot run one; see
                // the record's NOMINATION.
                (
                    TlsServerStream::Native(stream),
                    None,
                    "TLSv1.2".to_string(),
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
    let id = alloc_server_id(&mut reg);
    reg.server_streams.insert(id, entry);
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
pub(crate) fn rustls_server_handshake_over_stream(
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
pub(crate) fn rustls_client_handshake_over_stream(
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
enum PendingLayeredStream {
    Reused(TcpStream),
    DialFresh { host: String, port: u16 },
}

struct PendingLayeredSocket {
    stream: PendingLayeredStream,
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
    extra_roots: Vec<Vec<u8>>,
    use_java_trust_manager: bool,
    client_identity: Option<(String, String)>,
    enabled_ciphers: Vec<String>,
    /// Same deal for `SSLSocket.setEnabledProtocols()` — a caller may narrow
    /// the offered TLS versions any time before the deferred handshake
    /// starts, and (per real JSSE) the handshake must then genuinely fail
    /// when the peer supports none of them.
    enabled_protocols: Vec<String>,
    server_identity: Option<(String, String)>,
    host: String,
    client_mode: bool,
}

fn pending_layered_sockets() -> &'static Mutex<HashMap<i32, PendingLayeredSocket>> {
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
pub(crate) fn stash_pending_layered_socket(
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
        .map_err(|_| "--jdk-only refused a class this TLS context needs".to_string())?;
    // Server identity: same resolution `rustls_server_handshake_over_stream`'s
    // former caller used (this SSLContext's own identity, else the
    // process-wide runtime-configured one) — resolved here too so SERVER mode
    // never needs to touch `ssl_context` again.
    let server_identity = match ctx_identity(ctx, ssl_context)
        .map_err(|_| "--jdk-only refused a class this TLS context needs".to_string())?
    {
        Some(identity) => Some(identity),
        None => runtime_tls_identity().map(|identity| (identity.cert_pem, identity.key_pem)),
    };
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
        },
    );
    Ok(id)
}

/// `SSLSocket.setEnabledCipherSuites(String[])` on a still-pending layered
/// socket. A no-op if `pending_id` is unknown, same convention as
/// `set_pending_layered_socket_client_mode`.
pub(crate) fn set_pending_layered_socket_ciphers(pending_id: i32, ciphers: Vec<String>) {
    if let Some(p) = pending_layered_sockets().lock().get_mut(&pending_id) {
        p.enabled_ciphers = ciphers;
    }
}

/// `setEnabledProtocols(...)` on a still-pending layered socket — the
/// protocol-version sibling of `set_pending_layered_socket_ciphers`.
pub(crate) fn set_pending_layered_socket_protocols(pending_id: i32, protocols: Vec<String>) {
    if let Some(p) = pending_layered_sockets().lock().get_mut(&pending_id) {
        p.enabled_protocols = protocols;
    }
}

/// `setUseClientMode(false)` on a still-pending layered socket. A no-op if
/// `pending_id` is unknown (already handshaked, or was never a pending
/// layered socket) — matches this codebase's existing convention of ignoring
/// a client-mode change once a handshake is underway/complete.
pub(crate) fn set_pending_layered_socket_client_mode(pending_id: i32, client_mode: bool) {
    if let Some(p) = pending_layered_sockets().lock().get_mut(&pending_id) {
        p.client_mode = client_mode;
    }
}

/// Consume a pending layered socket and drive its handshake in whichever
/// role `setUseClientMode` last left it in (default client). Returns the RAW
/// rustls stream id — the caller applies `RUSTLS_SOCK_ID_BASE`, matching
/// every other rustls stream id in this module.
pub(crate) fn drive_pending_layered_handshake(pending_id: i32) -> Result<i32, String> {
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
fn rustls_classify_after_block(id: i32, result: std::io::Result<usize>) -> std::io::Result<usize> {
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

pub(crate) fn rustls_stream_read(id: i32, buf: &mut [u8]) -> std::io::Result<usize> {
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
pub(crate) fn rustls_stream_write(id: i32, data: &[u8]) -> std::io::Result<usize> {
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
pub(crate) fn rustls_stream_close(id: i32) {
    // Unregister under `sreg()`, then do the graceful shutdown outside it.
    let (client, server) = {
        let mut reg = sreg().lock();
        (
            reg.client_streams.remove(&id),
            reg.server_streams.remove(&id),
        )
    };
    // ─── WAKE THE PARKED PEER FIRST (W7-61) ──────────────────────────────────
    //
    // The sentence that used to stand here — "the entry is already
    // unregistered, so dropping our handle is sufficient — the socket closes
    // when the last `Arc` goes" — is precisely wrong in the case it was written
    // for. A thread parked in `rustls_stream_read` HOLDS an `Arc` on this
    // stream, so the last `Arc` does not go, the socket does not close, the
    // `try_lock` below always fails, and the reader waits forever. That is
    // W7-53's "four TLS sites" row, of which these two are half.
    //
    // `entry.raw` is a duplicate handle reachable without the stream mutex.
    // Shutting it down ends the underlying byte stream without freeing the
    // handle the parked thread is mid-syscall on (so it is not a
    // use-after-close) and without cutting a TLS record in half (the record
    // layer already has to handle a truncated connection; what it cannot
    // handle is a reader that returns mid-record and is then re-entered).
    //
    // PLATFORM, a contract rather than a measurement — no Linux arm was run:
    //   * Unix — `shutdown(SHUT_RDWR)` wakes a parked `recv` with EOF, and
    //     `rustls_classify_after_block` then reports the close instead of a
    //     spurious end-of-stream.
    //   * Windows — Winsock has no `shutdown` that aborts a pending blocking
    //     call, so for a reader ALREADY parked this is a no-op and that half of
    //     the row stays OPEN. Named, not quietly counted: the same reason
    //     W7-53 left the Windows pipe sink write open. A close that has not yet
    //     been raced into is still observed, because the classification runs on
    //     every return.
    for raw in [
        client.as_ref().and_then(|e| e.raw.as_ref()),
        server.as_ref().and_then(|e| e.raw.as_ref()),
    ]
    .into_iter()
    .flatten()
    {
        let _ = raw.shutdown(std::net::Shutdown::Both);
    }
    // `try_lock`: a peer parked in a blocking read on this SAME stream holds
    // the per-stream mutex, and waiting for it here would just relocate the
    // old global-lock stall. The graceful `close_notify` below is the nicety;
    // the `raw` shutdown above is the liveness guarantee and does not depend on
    // winning this lock.
    if let Some(e) = client.as_ref() {
        if let Some(mut s) = e.stream.try_lock() {
            s.conn.send_close_notify();
            let _ = s.flush();
        }
    }
    if let Some(e) = server.as_ref() {
        if let Some(mut guard) = e.stream.try_lock() {
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
}

/// Why this accepted server-side stream's TLS handshake failed, or `None` if
/// it did not — see [`TlsServerStream::HandshakeFailed`]. Lets the Java-facing
/// read/write natives raise `SSLHandshakeException` (what JSSE raises at the
/// first I/O on such a socket) rather than a bare `IOException`.
pub(crate) fn rustls_server_handshake_failure(id: i32) -> Option<String> {
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
pub(crate) fn rustls_listener_close(id: i32) {
    let mut reg = sreg().lock();
    if let Some(entry) = reg.listeners.remove(&id) {
        // Wake a thread parked in `rustls_server_accept`'s non-blocking poll
        // loop for this listener — see that function's doc comment.
        entry.closed.store(true, Ordering::SeqCst);
    }
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
pub(crate) fn rustls_client_peer_cert_chain_der(id: i32) -> Option<Vec<Vec<u8>>> {
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
/// It is the SYNTHETIC width and nothing else. Under `--jdk-only` — and in
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
const SSS_FIELDS: usize = 4;

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
struct SslServerSocketState {
    listener_id: i32,
    local_port: i32,
    closed: i32,
    /// Literal host address this listener is bound to — `"0.0.0.0"` for the
    /// two wildcard `createServerSocket` overloads, otherwise the
    /// `InetAddress.getHostAddress()` of the address the caller passed.
    /// Answers `getInetAddress()` / `getLocalSocketAddress()`.
    bind_address: String,
    /// `1` once a listener exists for this socket, `0` for one that
    /// `SSLServerSocketFactory.createServerSocket()` (the NO-ARG overload)
    /// made and nobody has bound. `isBound()` cannot be "we have a record of
    /// it" any more, because there is now a construction path that records an
    /// UNBOUND socket.
    bound: i32,
    /// `InetAddress.toString()` of the address actually bound, captured at
    /// creation time from the caller's own object, because that rendering
    /// (`hostname/literal`, hostname omitted when the address was built from a
    /// literal) is not reconstructible from the literal alone. Answers
    /// `toString()`.
    bind_display: String,
}

/// A miss in [`ssl_server_socket_states`] — an `SSLServerSocket` this module
/// did not create, which under `--jdk-only` cannot happen, because all three
/// `SSLServerSocketFactory.createServerSocket` overloads are intercepted.
///
/// The answers are deliberately the pessimistic ones the field fallbacks used
/// to produce for a totally unknown object (`closed = 1`, no listener), NOT
/// the ones a real unbound `ServerSocket` gives. A socket this file has no
/// record of is one it cannot accept on, and saying so is the honest answer.
fn ssl_server_socket_state_miss() -> SslServerSocketState {
    SslServerSocketState {
        listener_id: -1,
        local_port: 0,
        closed: 1,
        bound: 0,
        bind_address: String::new(),
        bind_display: String::new(),
    }
}

fn ssl_server_socket_states() -> &'static Mutex<HashMap<u64, SslServerSocketState>> {
    static STATES: OnceLock<Mutex<HashMap<u64, SslServerSocketState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ssl_server_socket_state(
    ctx: &dyn NativeContext,
    socket: ObjectRef,
) -> Option<SslServerSocketState> {
    ssl_server_socket_states()
        .lock()
        .get(&gc_stable_objref_key(ctx, socket))
        .cloned()
}

fn set_ssl_server_socket_state(
    ctx: &dyn NativeContext,
    socket: ObjectRef,
    state: SslServerSocketState,
) {
    ssl_server_socket_states()
        .lock()
        .insert(gc_stable_objref_key(ctx, socket), state);
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
fn sss_listener_identities() -> &'static Mutex<HashMap<i32, RuntimeTlsIdentity>> {
    static T: OnceLock<Mutex<HashMap<i32, RuntimeTlsIdentity>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-`SSLServerSocket` client-auth request as last set by
/// `setNeedClientAuth`/`setWantClientAuth`, keyed by `gc_stable_objref_key`.
/// Tuple is `(need, want)`, each 0/1; JSSE makes the two mutually exclusive.
fn sss_client_auth_states() -> &'static Mutex<HashMap<u64, (i32, i32)>> {
    static T: OnceLock<Mutex<HashMap<u64, (i32, i32)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Non-destructive client-CA source for the rebuild above. Deliberately NOT
/// `active_client_trust_roots()`: that one *takes* the thread-local selected
/// context slot, which a later client handshake on the same thread still
/// needs.
fn sss_client_ca_pem() -> String {
    let roots = selected_context_trust_roots().or_else(huc_default_trust_roots);
    trust_roots_pem(roots.as_ref())
}

/// Apply a `setNeedClientAuth`/`setWantClientAuth` request to the live
/// listener by rebuilding its `ServerConfig`.
///
/// Fails LOUDLY rather than silently when the request cannot be honoured: a
/// caller that asked for mandatory client authentication and got no exception
/// is entitled to assume it is in force. Turning client auth OFF is the state
/// the listener was already built in, so that direction never throws.
fn sss_apply_client_auth(
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
            let pem = sss_client_ca_pem();
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
const SSS_SOCK_HOST: usize = 0;
const SSS_SOCK_PORT: usize = 1;
const SSS_SOCK_TLSID: usize = 2;
const SSS_SOCK_CLOSED: usize = 3;
const SSS_SOCK_SESSION: usize = 4;
const SSS_SOCK_FIELDS: usize = 6;

/// Register T2.7's server-side and HTTPS natives. Called from
/// `register_phase68_natives` after `register_p68_ssl`.
pub(crate) fn register_t27_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    r.set_category(__prev_cat);
}

pub(crate) fn register_accepted_issuers(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
            let arr0 = ctx.new_ref_array(cratonvm_types::ClassId::new(0), ders.len());
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
        },
    );
    r.set_category(__prev_cat);
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
fn legacy_dsa_acceptor(cert_pem: &str, key_pem: &str) -> Result<SslAcceptor, String> {
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
pub(crate) fn pkcs8_algorithm_name(der: &[u8]) -> Option<&'static str> {
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
fn legacy_identity_hint(key_pem: &str) -> String {
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
const SSS_WILDCARD_BIND: &str = "0.0.0.0";
const SSS_WILDCARD_DISPLAY: &str = "0.0.0.0/0.0.0.0";

fn create_ssl_server_socket(
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
    let fallback = require_runtime_tls_identity()?;
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
    let id = {
        let mut reg = sreg().lock();
        let id = alloc_server_id(&mut reg);
        reg.listeners.insert(id, entry);
        id
    };
    // Remember the ingredients so `setNeedClientAuth`/`setWantClientAuth` can
    // rebuild this listener's config with a client verifier — see
    // `sss_listener_identities`.
    sss_listener_identities().lock().insert(id, identity);

    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocket", SSS_FIELDS)?;
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
fn sss_to_string(state: Option<&SslServerSocketState>) -> String {
    match state.filter(|state| state.bound != 0) {
        Some(state) => format!(
            "[SSL: ServerSocket[addr={},localport={}]]",
            state.bind_display, state.local_port
        ),
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
fn sss_closed_state(state: SslServerSocketState) -> SslServerSocketState {
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
fn ssl_server_bind_address(
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

fn register_sslserversocket(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
        },
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
        },
    );
    r.register(
        sssf,
        "createServerSocket",
        "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
        |ctx, args| {
            let port = args.get(1).and_then(|value| value.as_int()).unwrap_or(0);
            let (bind_address, bind_display) = ssl_server_bind_address(ctx, args, 3)?;
            create_ssl_server_socket(ctx, args, port, &bind_address, &bind_display)
        },
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
            let obj =
                try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocket", SSS_FIELDS)?;
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
        },
    );
    r.register(
        sssf,
        "getDefault",
        "()Ljavax/net/ServerSocketFactory;",
        |ctx, _args| {
            let obj =
                try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocketFactory", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        },
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
        Ok(Some(Value::Int(
            ssl_server_socket_state(ctx, this)
                .map(|state| state.local_port)
                .unwrap_or(ssl_server_socket_state_miss().local_port),
        )))
    });
    r.register(sss, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            ssl_server_socket_state(ctx, this)
                .map(|state| state.closed)
                .unwrap_or(ssl_server_socket_state_miss().closed),
        )))
    });
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
    });
    r.register(sss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
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
        let sock = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", SSS_SOCK_FIELDS)?;
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
        ctx.set_field(sock, SSS_SOCK_HOST, Value::Object(Some(host_str)));
        ctx.set_field(sock, SSS_SOCK_PORT, Value::Int(0));
        ctx.set_field(sock, SSS_SOCK_TLSID, Value::Int(tls_id));
        ctx.set_field(sock, SSS_SOCK_CLOSED, Value::Int(0));
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
        crate::net_phase_e::sock_set_for_create(ctx, sock, 0, tls_id);

        // 4-field synthetic session: proto, cipher, streamId, attrs (slot 3 —
        // see `sslsess_attrs_slot`, which is the width->slot table; this row
        // and the 8-field one are the only two with a dedicated attrs slot).
        // E31: these two comments used to name `SSLSESS_ATTRS_SLOT`, a doc
        // comment on a constant that has never existed in this tree — the
        // width rule they pointed at was only ever `num_fields - 1` open-coded
        // at five call sites. It exists now.
        let session0 = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 4)?;
        // GC: the four lines this replaces held `session` — and `p` — raw
        // across `create_string`, which can run a moving young collection. The
        // session's own pin is taken AFTER `sock_pin` so the single
        // `unpin_native_roots(sock_pin)` at the end of this body still
        // truncates both; the same discipline the ALPN stash and
        // `new13_finish_socket` already use here.
        let session_pin = ctx.pin_native_root(session0);
        let p = ctx.create_string(&proto);
        let session = ctx.read_native_pin(session_pin, session0);
        ctx.set_field(session, 0, Value::Object(Some(p)));
        let c = ctx.create_string(&cipher);
        let session = ctx.read_native_pin(session_pin, session0);
        ctx.set_field(session, 1, Value::Object(Some(c)));
        // Offset id here too: the session accessors subtract
        // `RUSTLS_SOCK_ID_BASE` before asking `rustls_session_info`, and pass
        // anything below it to the native-tls lookup instead.
        ctx.set_field(session, 2, Value::Int(tls_id));
        let sock = ctx.read_native_pin(sock_pin, sock);
        ctx.set_field(sock, SSS_SOCK_SESSION, Value::Object(Some(session)));
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
    });
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
    });
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
    });
    r.register(sss, "setWantClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        sss_apply_client_auth(ctx, this, false, on)
    });
    // Paired getters: `javax.net.ssl.SSLServerSocket` declares both abstract,
    // so without a native an ordinary read-back throws AbstractMethodError.
    // Now that the setters keep real state, report it.
    r.register(sss, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = gc_stable_objref_key(ctx, this);
        let need = sss_client_auth_states()
            .lock()
            .get(&key)
            .map(|(need, _)| *need)
            .unwrap_or(0);
        Ok(Some(Value::Int(need)))
    });
    r.register(sss, "getWantClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = gc_stable_objref_key(ctx, this);
        let want = sss_client_auth_states()
            .lock()
            .get(&key)
            .map(|(_, want)| *want)
            .unwrap_or(0);
        Ok(Some(Value::Int(want)))
    });
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
        },
    );
    r.register(
        sss,
        "getEnabledProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let list = lookup_sss_enabled_protocols(ctx, this)
                .unwrap_or_else(|| vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()]);
            // GC NOTE: `create_string` allocates, so the array is rooted
            // across the loop — see `x509_manager::materialize_string_array`.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let arr = scope.new_ref_array(cratonvm_types::ClassId::new(0), list.len());
            let arr_h = scope.root(arr);
            for (i, p) in list.iter().enumerate() {
                let s = scope.create_string(p);
                let arr = scope.get(&arr_h);
                scope.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
        },
    );
    r.register(
        sss,
        "getSupportedProtocols",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // GC NOTE: rooted across the two `create_string` allocations —
            // see `x509_manager::materialize_string_array`.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let arr = scope.new_ref_array(cratonvm_types::ClassId::new(0), 2);
            let arr_h = scope.root(arr);
            let s1 = scope.create_string("TLSv1.3");
            let s1_h = scope.root(s1);
            let s2 = scope.create_string("TLSv1.2");
            let s1 = scope.get(&s1_h);
            let arr = scope.get(&arr_h);
            scope.set_array_element(arr, 0, Value::Object(Some(s1)));
            scope.set_array_element(arr, 1, Value::Object(Some(s2)));
            Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
        },
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
    });
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
        },
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
            sss_local_socket_address(ctx, &state)
        },
    );
    r.register(sss, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let text = sss_to_string(ssl_server_socket_state(ctx, this).as_ref());
        let text = ctx.create_string(&text);
        Ok(Some(Value::Object(Some(text))))
    });

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
        },
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
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), list.len());
            let pin = ctx.pin_native_root(arr);
            for (i, name) in list.iter().enumerate() {
                let name = ctx.create_string(name);
                let arr = ctx.read_native_pin(pin, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(name)));
            }
            let arr = ctx.read_native_pin(pin, arr);
            ctx.unpin_native_roots(pin);
            Ok(Some(Value::Object(Some(arr))))
        },
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
        },
    );
    r.register(sss, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sss_mode_state(ctx, this).0)))
    });
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
    });
    r.register(sss, "getEnableSessionCreation", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sss_mode_state(ctx, this).1)))
    });
    r.register(sss, "setEnableSessionCreation", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = i32::from(args.get(1).and_then(|value| value.as_int()).unwrap_or(0) != 0);
        let key = gc_stable_objref_key(ctx, this);
        let mut table = sss_mode_states().lock();
        let entry = table.entry(key).or_insert(SSS_MODE_DEFAULT);
        entry.1 = on;
        Ok(None)
    });
    r.set_category(__prev_cat);
}

/// `(getUseClientMode, getEnableSessionCreation)` on a socket nobody has
/// configured. MEASURED on HotSpot 25.0.3+9 (`G25Probe`): a server socket is
/// not in client mode, and session creation is on.
const SSS_MODE_DEFAULT: (i32, i32) = (0, 1);

/// Per-`SSLServerSocket` `(useClientMode, enableSessionCreation)`, keyed by
/// [`gc_stable_objref_key`]. Same rationale as `sss_client_auth_states`: the
/// real `java.net.ServerSocket` layout has no slot that means either of these
/// (see [`SSS_FIELDS`]), and writing into one that means something else is the
/// defect this whole block exists to undo.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Every acquisition takes
/// the guard after `gc_stable_objref_key` has already produced the key, and
/// holds it only across a `HashMap::entry` on a `(i32, i32)`.
fn sss_mode_states() -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, (i32, i32)>>
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

fn sss_mode_state(ctx: &dyn NativeContext, socket: ObjectRef) -> (i32, i32) {
    let key = gc_stable_objref_key(ctx, socket);
    sss_mode_states()
        .lock()
        .get(&key)
        .copied()
        .unwrap_or(SSS_MODE_DEFAULT)
}

/// Per-`SSLServerSocket` `setEnabledCipherSuites` list. Absent means "never
/// narrowed", which reads back as the full supported list — not as an empty
/// one, which would say this socket can negotiate nothing.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both acquisitions are one
/// statement over an already-built key and an already-built `Vec<String>`.
fn sss_enabled_suites_table(
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

fn stash_sss_enabled_suites(ctx: &dyn NativeContext, socket: ObjectRef, suites: Vec<String>) {
    let key = gc_stable_objref_key(ctx, socket);
    sss_enabled_suites_table().lock().insert(key, suites);
}

fn lookup_sss_enabled_suites(ctx: &dyn NativeContext, socket: ObjectRef) -> Option<Vec<String>> {
    let key = gc_stable_objref_key(ctx, socket);
    sss_enabled_suites_table().lock().get(&key).cloned()
}

/// A fresh `java.net.InetSocketAddress` for this listener's bind address and
/// local port — the object `getLocalSocketAddress()` returns and the one
/// `getInetAddress()` unwraps.
///
/// Built through the REAL `InetSocketAddress` constructor bytecode rather than
/// transcribed, so the `hostname/literal:port` rendering, the IPv6 bracketing
/// and the resolved/unresolved distinction are the JDK's own.
fn sss_local_socket_address(
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
fn sss_accept_timeout(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<std::time::Duration> {
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
fn sss_enabled_protocols_table() -> &'static Mutex<HashMap<u64, Vec<String>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<String>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stash_sss_enabled_protocols(ctx: &dyn NativeContext, socket: ObjectRef, protocols: Vec<String>) {
    let key = gc_stable_objref_key(ctx, socket);
    sss_enabled_protocols_table().lock().insert(key, protocols);
}

fn lookup_sss_enabled_protocols(ctx: &dyn NativeContext, socket: ObjectRef) -> Option<Vec<String>> {
    let key = gc_stable_objref_key(ctx, socket);
    sss_enabled_protocols_table().lock().get(&key).cloned()
}

/// Side-table storing ALPN protocols per SSLSocket objectref. Used so
/// `SSLSocket.getApplicationProtocol()` can return real values without
/// widening the synthetic-field layout (which would break every
/// SSLSocket allocation site in phases_late).
fn sock_alpn_table() -> &'static Mutex<HashMap<u64, String>> {
    static T: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stash_sock_alpn(ctx: &dyn NativeContext, sock: ObjectRef, alpn: String) {
    let key = gc_stable_objref_key(ctx, sock);
    sock_alpn_table().lock().insert(key, alpn);
}

fn lookup_sock_alpn(ctx: &dyn NativeContext, sock: ObjectRef) -> Option<String> {
    let key = gc_stable_objref_key(ctx, sock);
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
/// `ctx.identity_hash_code` is the VM's real, GC-stable identity hash,
/// computed once and pinned for an object's lifetime regardless of later
/// moves.
fn gc_stable_objref_key(ctx: &dyn NativeContext, o: ObjectRef) -> u64 {
    ctx.identity_hash_code(o) as u32 as u64
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
fn register_client_socket_mode_accessors(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let ss = "javax/net/ssl/SSLSocket";

    // A client socket IS in client mode, and session creation is on: the
    // mirror image of `SSS_MODE_DEFAULT`, whose first element is 0 because a
    // SERVER socket is not.
    r.register(ss, "getEnableSessionCreation", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = gc_stable_objref_key(ctx, this);
        let state = sss_mode_states()
            .lock()
            .get(&key)
            .copied()
            .unwrap_or((1, 1));
        Ok(Some(Value::Int(state.1)))
    });
    // A socket from `SSLSocketFactory.createSocket()` IS in client mode:
    // measured `clientMode=true` on HotSpot where this VM answered false,
    // because the only `getUseClientMode` registered was the SERVER socket's
    // and its default is the opposite.
    r.register(ss, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = gc_stable_objref_key(ctx, this);
        let state = sss_mode_states()
            .lock()
            .get(&key)
            .copied()
            .unwrap_or((1, 1));
        Ok(Some(Value::Int(state.0)))
    });
    r.register(ss, "setUseClientMode", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = i32::from(args.get(1).and_then(|value| value.as_int()).unwrap_or(0) != 0);
        let key = gc_stable_objref_key(ctx, this);
        let mut table = sss_mode_states().lock();
        let entry = table.entry(key).or_insert((1, 1));
        entry.0 = on;
        Ok(None)
    });
    r.register(ss, "setEnableSessionCreation", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = i32::from(args.get(1).and_then(|value| value.as_int()).unwrap_or(0) != 0);
        let key = gc_stable_objref_key(ctx, this);
        let mut table = sss_mode_states().lock();
        let entry = table.entry(key).or_insert((1, 1));
        entry.1 = on;
        Ok(None)
    });

    r.register(
        ss,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            // Null first, then membership: the order is observable, and
            // `setEnabledCipherSuites(null)` reports "CipherSuites cannot be
            // null" on HotSpot rather than complaining about a null suite.
            let Some(Value::Object(Some(arr))) = args.get(1) else {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "CipherSuites cannot be null".into(),
                }
                .into());
            };
            for i in 0..ctx.array_length(*arr) {
                let name = match ctx.get_array_element(*arr, i) {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                };
                let name = name.unwrap_or_default();
                if !SUPPORTED_CIPHER_SUITE_NAMES.contains(&name.as_str()) {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("Unsupported CipherSuite: {name}"),
                    }
                    .into());
                }
            }
            Ok(None)
        },
    );
    r.register(
        ss,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let Some(Value::Object(Some(arr))) = args.get(1) else {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Protocols cannot be null".into(),
                }
                .into());
            };
            for i in 0..ctx.array_length(*arr) {
                let name = match ctx.get_array_element(*arr, i) {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                };
                let name = name.unwrap_or_default();
                // The protocol names this stack reports through
                // `getSupportedProtocols`, plus the legacy spellings JSSE
                // still names. Anything else is a caller's typo, and HotSpot
                // says so rather than ignoring it.
                const KNOWN: &[&str] = &[
                    "TLSv1.3",
                    "TLSv1.2",
                    "TLSv1.1",
                    "TLSv1",
                    "SSLv3",
                    "SSLv2Hello",
                ];
                if !KNOWN.contains(&name.as_str()) {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("Unsupported protocol: {name}"),
                    }
                    .into());
                }
            }
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

fn register_alpn_accessor(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    r.set_category(__prev_cat);
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
fn register_socket_handshake_session(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "javax/net/ssl/SSLSocket",
        "getHandshakeSession",
        "()Ljavax/net/ssl/SSLSession;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.set_category(__prev_cat);
}

fn register_https_url_connection(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    /// Publishing into `huc_default_factory_slot` (rather than caching in a
    /// new static) is what keeps a later explicit `setDefaultSSLSocketFactory`
    /// winning, and the slot is already a GC root.
    fn huc_default_factory_or_publish(
        ctx: &mut dyn cratonvm_native_api::NativeContext,
    ) -> Result<ObjectRef, MethodCallFailed> {
        if let Some(f) = huc_default_ssl_socket_factory() {
            return Ok(f);
        }
        let obj = default_ssl_socket_factory_obj(ctx)?;
        set_huc_default_ssl_socket_factory(obj);
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
        },
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
        // restriction never ran at all. See `huc_default_factory_slot`.
        set_huc_default_ssl_socket_factory(factory);
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
                publish_default_ssl_socket_factory(ctx, *f)?;
            }
            Ok(None)
        },
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
            capture_huc_client_identity(ctx, factory, Some(connection))?;
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
        },
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
        },
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
        },
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
                ctx.set_static_field(cid, idx, Value::Object(Some(verifier)));
            }
            Ok(None)
        },
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
        },
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
        },
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
        },
    );
    // Cipher suite / peer principal accessors come from the underlying
    // SSLSession/SSLSocket registrations in phases_late; we do not duplicate
    // them here to avoid conflicting registrations.
    r.set_category(__prev_cat);
}

fn register_self_test(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
                Some(id) => {
                    run_loopback_self_test(&id.cert_pem, &id.key_pem, id.client_ca_pem.as_deref())
                }
                None => Err("No TLS key/cert configured; set javax.net.ssl.keyStore".to_string()),
            };
            let s = match result {
                Ok(msg) => ctx.create_string(&msg),
                Err(e) => ctx.create_string(&format!("ERR: {}", e)),
            };
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.set_category(__prev_cat);
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

/// An EC server identity generated by `java.security.KeyPairGenerator("EC")`
/// must build a rustls signing key.
///
/// The JDK emits PKCS#8 whose inner SEC1 `ECPrivateKey` carries neither
/// `parameters [0]` nor `publicKey [1]` — 67 bytes for P-256, byte-identical on
/// HotSpot and CratonVM (verified against JDK 25). `ring` can only build an
/// `EcdsaKeyPair` from a PKCS#8 that HAS the public key, and reports the refusal
/// as the generic "failed to parse private key as RSA, ECDSA, or EdDSA" — so
/// every JDK-generated EC server identity was unusable, and netty's
/// `Http2MultiplexTransportTest.testFireChannelReadAfterHandshakeSuccess_JDK`
/// hung forever waiting on a handshake that could never complete.
///
/// Both halves are pinned: that the stripped shape is genuinely rejected
/// WITHOUT the repair (so removing the splice fails this module, rather than
/// leaving a test that cannot fail), and that it is accepted with it.
#[cfg(test)]
mod ec_pkcs8_v1_identity_tests {
    use super::*;

    /// P-256 identity, generated once with openssl and frozen here so these
    /// tests need no crypto dependency and run on every platform. The key is
    /// PKCS#8 WITH `publicKey [1]`; `strip_to_jdk_shape` reduces it to what the
    /// JDK emits.
    const P256_KEY: &str = "\
        308187020100301306072a8648ce3d020106082a8648ce3d030107046d306b0201010420\
        52938df7e0c9a16537f034339c7c7359eced61a35d1b4c87760275ff735ee055a1440342\
        00040e06bf5c39a8aa566ca83cb86b72d7e38686fee8ce84850064372f42433a7ad3cd49\
        da99d89ec101763121462a25f8e6c18c90fb7eb089fcfe0e73aa0d743c42";
    const P256_CRT: &str = "\
        3082017c30820123a00302010202141f64638ec784227782eb3d08509bde7d8e9fc10c30\
        0a06082a8648ce3d04030230143112301006035504030c096c6f63616c686f7374301e17\
        0d3236303831323138303333365a170d3336303830393138303333365a30143112301006\
        035504030c096c6f63616c686f73743059301306072a8648ce3d020106082a8648ce3d03\
        0107034200040e06bf5c39a8aa566ca83cb86b72d7e38686fee8ce84850064372f42433a\
        7ad3cd49da99d89ec101763121462a25f8e6c18c90fb7eb089fcfe0e73aa0d743c42a353\
        3051301d0603551d0e0416041468f6f21e5636c47e25780b9b832afe4de707a302301f06\
        03551d2304183016801468f6f21e5636c47e25780b9b832afe4de707a302300f0603551d\
        130101ff040530030101ff300a06082a8648ce3d0403020347003044022001ce1e112093\
        114d88086e2105680bb39606ba9c5f67332da35764aafc8d977402204d049c6b89003aa0\
        5b34697a1a6380393dba365220e82d3f25a6b368d1807289";

    /// P-384 identity — the splice must be curve-agnostic, since it copies the
    /// point out of the certificate rather than computing it.
    const P384_KEY: &str = "\
        3081b6020100301006072a8648ce3d020106052b8104002204819e30819b020101043004\
        0e7e93856a01d61ca3d12ac7adafd39eccdb844fbeeda287df27b950794f39f4d8277cea\
        3f4beb35df0ce4e5dec82aa16403620004e185c328a18debe1215987b59333173d761ddc\
        fc5d5a6172461f40cb8b5e0c2d350792d8008d0b653c81f93c44f8ff8d1b28751f84772b\
        90662213b6bf2d90fb31f60027c18a38d23d7cc2bd80644a628de877b04077416b879732\
        ead4163238";
    const P384_CRT: &str = "\
        308201ba30820140a00302010202145594e93072003edfb390fffc595b84db6628df7c30\
        0a06082a8648ce3d04030230143112301006035504030c096c6f63616c686f7374301e17\
        0d3236303831323138303333365a170d3336303830393138303333365a30143112301006\
        035504030c096c6f63616c686f73743076301006072a8648ce3d020106052b8104002203\
        620004e185c328a18debe1215987b59333173d761ddcfc5d5a6172461f40cb8b5e0c2d35\
        0792d8008d0b653c81f93c44f8ff8d1b28751f84772b90662213b6bf2d90fb31f60027c1\
        8a38d23d7cc2bd80644a628de877b04077416b879732ead4163238a3533051301d060355\
        1d0e0416041402e7bb3ae8de78e5dae7449f71da2296fd395874301f0603551d23041830\
        16801402e7bb3ae8de78e5dae7449f71da2296fd395874300f0603551d130101ff040530\
        030101ff300a06082a8648ce3d040302036800306502301806748b77941e85ada3cbe438\
        5e6a5878ffd9b3a1950ffc0265ff13ef21958816d6811b8bffcd3c35f4d0b63f185b1c02\
        3100afa5d9b1a03c4df25a3807ff3a4408ba7efa1ffd9d6e68aa13429ee73d39b23f3d61\
        1e36f3ed53be1379ed00737f5d44";

    fn unhex(s: &str) -> Vec<u8> {
        let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (0..clean.len() / 2)
            .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    /// Reduce a PKCS#8 EC key's inner SEC1 to `version` + `privateKey` —
    /// exactly what a stock JDK's `getEncoded()` produces.
    fn strip_to_jdk_shape(der: &[u8]) -> Vec<u8> {
        let (outer, _) = der_tlv(der, 0).unwrap();
        let ver_end = der_tlv_end(der, outer).unwrap();
        let alg_end = der_tlv_end(der, ver_end).unwrap();
        let (oct_body, _) = der_tlv(der, alg_end).unwrap();
        let (sec1_body, _) = der_tlv(der, oct_body).unwrap();
        let iv_end = der_tlv_end(der, sec1_body).unwrap();
        let ipk_end = der_tlv_end(der, iv_end).unwrap();

        let inner = der[sec1_body..ipk_end].to_vec();
        let mut body = Vec::new();
        body.extend_from_slice(&der[outer..alg_end]);
        body.extend_from_slice(&der_tlv_encode(0x04, &der_tlv_encode(0x30, &inner)));
        der_tlv_encode(0x30, &body)
    }

    /// `(parameters[0] present, publicKey[1] present)` for a PKCS#8 EC key.
    fn inner_optionals(der: &[u8]) -> (bool, bool) {
        let (outer, _) = der_tlv(der, 0).unwrap();
        let ver_end = der_tlv_end(der, outer).unwrap();
        let alg_end = der_tlv_end(der, ver_end).unwrap();
        let (oct_body, _) = der_tlv(der, alg_end).unwrap();
        let (sec1_body, sec1_end) = der_tlv(der, oct_body).unwrap();
        let mut p = der_tlv_end(der, der_tlv_end(der, sec1_body).unwrap()).unwrap();
        let (mut a, mut b) = (false, false);
        while p < sec1_end {
            match der[p] {
                0xa0 => a = true,
                0xa1 => b = true,
                _ => {}
            }
            p = der_tlv_end(der, p).unwrap();
        }
        (a, b)
    }

    fn accepted_by_ring(pkcs8: &[u8]) -> bool {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.to_vec()));
        rustls::crypto::ring::sign::any_supported_type(&key).is_ok()
    }

    fn round_trip(key_hex: &str, crt_hex: &str, label: &str) {
        let full = unhex(key_hex);
        let cert = unhex(crt_hex);
        assert!(
            inner_optionals(&full).1,
            "{label}: fixture must carry publicKey[1] to be a valid control"
        );
        assert!(
            accepted_by_ring(&full),
            "{label}: control — ring must accept the unmodified fixture"
        );

        let jdk = strip_to_jdk_shape(&full);
        assert_eq!(
            inner_optionals(&jdk),
            (false, false),
            "{label}: stripped key must carry no optional fields"
        );
        // The bug, pinned. If this ever starts passing, ring learned to derive
        // the public key and the splice below can be deleted.
        assert!(
            !accepted_by_ring(&jdk),
            "{label}: ring accepted a PKCS#8 EC key with no publicKey"
        );

        let repaired = ec_pkcs8_splice_public_key(&jdk, &cert)
            .unwrap_or_else(|| panic!("{label}: splice declined a stripped EC key"));
        assert!(
            inner_optionals(&repaired).1,
            "{label}: repaired key must carry publicKey[1]"
        );
        assert!(
            accepted_by_ring(&repaired),
            "{label}: ring must accept the repaired key"
        );
    }

    #[test]
    fn jdk_shaped_p256_identity_is_repaired_from_its_certificate() {
        round_trip(P256_KEY, P256_CRT, "P-256");
    }

    #[test]
    fn jdk_shaped_p384_identity_is_repaired_from_its_certificate() {
        round_trip(P384_KEY, P384_CRT, "P-384");
    }

    /// The mTLS `KeyManager` resolver is the one identity path that never
    /// parses PEM: `JavaKeyManagerResolver::resolve_via_java` takes DER
    /// straight from `km_alias_material` and hands it to
    /// `CertifiedKey::from_der`. It was missed when the repair first landed, so
    /// a JDK-generated EC *client* certificate delivered through a Java
    /// `KeyManager` still resolved to nothing.
    ///
    /// Pinned through `CertifiedKey::from_der` rather than
    /// `any_supported_type`, because that is the call the resolver makes — a
    /// test against the lower-level entry point would not have caught the
    /// missing call site either.
    #[test]
    fn a_key_manager_supplied_jdk_ec_identity_is_repaired_too() {
        let cert = CertificateDer::from(unhex(P256_CRT));
        let jdk = strip_to_jdk_shape(&unhex(P256_KEY));
        let provider = rustls::crypto::ring::default_provider();

        // Control: the shape `resolve_via_java` used to build is genuinely
        // rejected, so the positive half below cannot pass vacuously.
        assert!(
            CertifiedKey::from_der(
                vec![cert.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(jdk.clone())),
                &provider,
            )
            .is_err(),
            "control — CertifiedKey::from_der must reject the stripped JDK shape"
        );

        // The function `resolve_via_java` calls, not a re-creation of it.
        assert!(
            certified_key_from_der_repairing_ec(vec![cert], jdk, &provider).is_ok(),
            "the KeyManager identity path must repair the JDK EC key it is handed"
        );
    }

    #[test]
    fn splice_declines_a_key_that_already_has_a_public_key() {
        assert!(
            ec_pkcs8_splice_public_key(&unhex(P256_KEY), &unhex(P256_CRT)).is_none(),
            "a key ring already accepts must be left byte-identical"
        );
    }

    #[test]
    fn splice_declines_when_the_certificate_cannot_lend_a_matching_point() {
        // A P-384 certificate must not have its point spliced into a P-256 key:
        // the splice keys off the cert, so a mismatched pair must be refused by
        // ring rather than silently producing an identity that signs wrong.
        let jdk_p256 = strip_to_jdk_shape(&unhex(P256_KEY));
        let spliced = ec_pkcs8_splice_public_key(&jdk_p256, &unhex(P384_CRT))
            .expect("the P-384 cert does carry an EC point");
        assert!(
            !accepted_by_ring(&spliced),
            "ring must reject a public key that does not match the private scalar"
        );
    }

    #[test]
    fn splice_declines_non_ec_keys() {
        // An RSA PKCS#8 (any bytes with the RSA algorithm OID) must be left
        // alone — those parse fine and rewriting them could only break them.
        let rsa_alg_pkcs8 = unhex("30820102020100300d06092a864886f70d0101010500048200ec3082");
        assert!(ec_pkcs8_splice_public_key(&rsa_alg_pkcs8, &unhex(P256_CRT)).is_none());
    }

    #[test]
    fn splice_declines_garbage_without_panicking() {
        let cert = unhex(P256_CRT);
        for bad in [
            &b""[..],
            &b"\x30"[..],
            &b"\x30\x82"[..],
            &b"\x30\x03\x02\x01\x00"[..],
            &b"\x02\x01\x00"[..],
            &[0x30, 0x84, 0xff, 0xff, 0xff, 0xff][..],
            &[0x30, 0x80, 0x02, 0x01, 0x00][..],
        ] {
            assert!(ec_pkcs8_splice_public_key(bad, &cert).is_none());
        }
        let jdk = strip_to_jdk_shape(&unhex(P256_KEY));
        for bad_cert in [
            &b""[..],
            &b"\x30\x03\x02\x01\x00"[..],
            &[0x30, 0x84, 0xff, 0xff, 0xff, 0xff][..],
        ] {
            assert!(ec_pkcs8_splice_public_key(&jdk, bad_cert).is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::*;
    use super::*;
    use cratonvm_native_api::NativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ObjectRef;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    /// A nested `set_active_native_context` must RESTORE the outer window, not
    /// clear it.
    ///
    /// There are two publishers now — `http_url_connection::perform` on the
    /// client path and `x509_manager::do_check_trusted` on the server path —
    /// and on a client connection that validates a chain they nest. With the
    /// old clear-to-`None` drop, the inner guard's return would leave the outer
    /// frame with no window, and every later `with_active_native_context` there
    /// would answer `None`. For `JavaKeyManagerResolver::resolve` that is not a
    /// crash, it is "no client certificate" — a silent wrong answer, on the one
    /// path whose whole job is to produce one.
    ///
    /// Asserted through `with_active_native_context`, the real reader, rather
    /// than by inspecting the thread-local: that is what every consumer
    /// actually calls.
    #[test]
    fn a_nested_active_native_context_restores_the_outer_one() {
        use crate::test_utils::MockNativeContext;
        let mut outer = MockNativeContext::new();
        let mut inner = MockNativeContext::new();

        assert!(
            super::with_active_native_context(|_| ()).is_none(),
            "no window should be published before the first guard"
        );
        let outer_guard = super::set_active_native_context(&mut outer);
        let outer_ptr = ACTIVE_TLS_NATIVE_CTX.with(|c| c.get());
        assert!(outer_ptr.is_some(), "the outer guard must publish a window");
        {
            let _inner_guard = super::set_active_native_context(&mut inner);
            let inner_ptr = ACTIVE_TLS_NATIVE_CTX.with(|c| c.get());
            assert!(inner_ptr.is_some());
            assert!(
                !std::ptr::addr_eq(inner_ptr.unwrap(), outer_ptr.unwrap()),
                "the inner guard must publish ITS context while it is alive"
            );
        }
        assert!(
            super::with_active_native_context(|_| ()).is_some(),
            "the inner guard cleared the window instead of restoring it — the \
             enclosing frame is now running with no published context, and every \
             `with_active_native_context` in it silently answers None"
        );
        assert!(
            std::ptr::addr_eq(
                ACTIVE_TLS_NATIVE_CTX.with(|c| c.get()).unwrap(),
                outer_ptr.unwrap()
            ),
            "the restored window must be the OUTER context, not some other one"
        );
        drop(outer_guard);
        assert!(
            super::with_active_native_context(|_| ()).is_none(),
            "the outermost guard must still clear the window on the way out"
        );
    }

    /// A server session cache is per `SSLContext` **and per client-auth
    /// policy** — a connection asking for a client certificate must not be
    /// able to resume one that did not.
    ///
    /// Resuming across that boundary replays a handshake that sent no
    /// `CertificateRequest`, so the client is never asked and the request is
    /// silently cancelled. That is what defeated `wants_deferred_client_auth`
    /// and produced `TestClientCert`'s whole failing set: the second engine
    /// really did offer client auth (`engine_begin request=true`, measured)
    /// and `JavaKeyManagerResolver::resolve` was still called zero times.
    ///
    /// Asserted on `Arc::ptr_eq`, which is the property that matters — two
    /// distinct caches, not merely two lookups. The same-policy case is
    /// asserted too, because a partition that never shares would silently
    /// disable resumption altogether and still pass a difference-only check.
    #[test]
    fn a_server_session_cache_is_partitioned_by_client_auth_policy() {
        let key = 0x5eed_0000_0000_0001u64;
        let without = super::ctx_server_session_store(key, false);
        let with = super::ctx_server_session_store(key, true);
        assert!(
            !Arc::ptr_eq(&without, &with),
            "a client-auth connection shares the no-client-auth session cache, so it \
             can resume a session that carries no client certificate — the resumption \
             then cancels the CertificateRequest and the peer is never asked"
        );
        assert!(
            Arc::ptr_eq(&without, &super::ctx_server_session_store(key, false)),
            "same context, same policy must share ONE cache — otherwise this \
             partition has disabled server-side resumption instead of scoping it"
        );
        assert!(
            Arc::ptr_eq(&with, &super::ctx_server_session_store(key, true)),
            "same context, same policy must share ONE cache (client-auth side)"
        );
    }

    /// The exact PKCS#8 key CratonVM lifts out of Spring Boot's
    /// `spring-boot-ldap` test keystore
    /// (`.../ldap/autoconfigure/embedded/test.jks`, alias `mykey`, 335 bytes),
    /// captured with `CRATONVM_DBG=tls-hs` on 2026-08-11. Truncated to the
    /// header — the algorithm OID is all this test reads.
    const LDAP_TEST_JKS_DSA_KEY_PREFIX: &[u8] = &[
        0x30, 0x82, 0x01, 0x4b, 0x02, 0x01, 0x00, 0x30, 0x82, 0x01, 0x2c, 0x06, 0x07, 0x2a, 0x86,
        0x48, 0xce, 0x38, 0x04, 0x01, 0x30, 0x82, 0x01, 0x1f, 0x02, 0x81, 0x00,
    ];

    /// `EmbeddedLdapAutoConfigurationTests.whenSslBundleIsConfiguredLdapsListenerIsConfigured`
    /// fails on Windows with rustls's generic "failed to parse private key as
    /// RSA, ECDSA, or EdDSA", which says nothing about why. It is a DSA key, and
    /// naming that is what separates "broken keystore" from "no TLS backend on
    /// this platform speaks DHE_DSS".
    #[test]
    fn ldap_test_keystore_key_is_reported_as_dsa() {
        assert_eq!(
            pkcs8_algorithm_name(LDAP_TEST_JKS_DSA_KEY_PREFIX),
            Some("DSA")
        );
    }

    #[test]
    fn pkcs8_algorithm_name_reads_the_algorithm_oid() {
        // Minimal PKCS#8 prefixes: SEQUENCE { INTEGER 0, SEQUENCE { OID .. } }
        let rsa = [
            0x30u8, 0x10, 0x02, 0x01, 0x00, 0x30, 0x0b, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7,
            0x0d, 0x01, 0x01, 0x01,
        ];
        let ec = [
            0x30u8, 0x0e, 0x02, 0x01, 0x00, 0x30, 0x09, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d,
            0x02, 0x01,
        ];
        let ed = [
            0x30u8, 0x0a, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70,
        ];
        assert_eq!(pkcs8_algorithm_name(&rsa), Some("RSA"));
        assert_eq!(pkcs8_algorithm_name(&ec), Some("EC"));
        assert_eq!(pkcs8_algorithm_name(&ed), Some("Ed25519"));
        // Not a key at all, and a truncated one: both must decline rather than
        // name an algorithm the bytes do not carry.
        assert_eq!(pkcs8_algorithm_name(b"not der"), None);
        assert_eq!(pkcs8_algorithm_name(&rsa[..6]), None);
    }

    /// Guard so tests that mutate the global `RUNTIME_TLS_IDENTITY` slot
    /// don't race with each other. Each test acquires the lock for its
    /// duration; the previous slot value is restored on drop.
    static IDENTITY_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn der_identity_to_pem_preserves_private_key_encoding() {
        // Minimal DER envelopes are sufficient for the label sniffer: its
        // decision only depends on the outer sequence, version, and next tag.
        let pkcs8 = [0x30, 0x07, 0x02, 0x01, 0x00, 0x30, 0x02, 0x06, 0x00];
        let pkcs1_rsa = [0x30, 0x08, 0x02, 0x01, 0x00, 0x02, 0x03, 0x01, 0x02, 0x03];
        let sec1_ec = [0x30, 0x05, 0x02, 0x01, 0x00, 0x04, 0x00];

        assert!(der_identity_to_pem(&pkcs8, &[])
            .1
            .starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(der_identity_to_pem(&pkcs1_rsa, &[])
            .1
            .starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(der_identity_to_pem(&sec1_ec, &[])
            .1
            .starts_with("-----BEGIN EC PRIVATE KEY-----"));
    }

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
            let lock = IDENTITY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let prev = super::runtime_tls_identity();
            super::set_runtime_tls_identity(Some(identity));
            IdentityGuard { _lock: lock, prev }
        }

        fn install_none() -> Self {
            let lock = IDENTITY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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

    fn fake_object_ref(tag: usize) -> ObjectRef {
        let ptr = (0x1000_0000usize + tag * 0x1000) as *mut u8;
        unsafe { ObjectRef::from_raw(ptr) }
    }

    // -------------------------------------------------------------------
    // bb_view buffer-shape resolution (Reactor-Netty SSLEngine
    // BUFFER_UNDERFLOW fix, 2026-07-07). The mock maps real-JDK
    // java.nio.Buffer field names for "java/nio/*ByteBuffer" classes:
    // mark@0, position@1, limit@2, capacity@3, address@4, hb@5, offset@6.
    // -------------------------------------------------------------------

    /// Real-JDK HeapByteBuffer shape with a nonzero arrayOffset (a sliced
    /// or duplicated view, e.g. a Netty pooled heap buffer): logical index
    /// 0 lives at `hb[offset]`, so reads must add the offset.
    #[test]
    fn bb_view_heap_named_respects_array_offset() {
        let mut ctx = crate::test_utils::mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        for i in 0..16 {
            ctx.set_array_element(arr, i, Value::Int(i as i32));
        }
        let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/nio/HeapByteBuffer", 8).unwrap();
        ctx.set_field_by_name(bb, "hb", Value::Object(Some(arr)));
        ctx.set_field_by_name(bb, "position", Value::Int(1));
        ctx.set_field_by_name(bb, "limit", Value::Int(4));
        ctx.set_field_by_name(bb, "capacity", Value::Int(6));
        ctx.set_field_by_name(bb, "offset", Value::Int(10));

        let mut out = Vec::new();
        let n = bb_read_into(&mut ctx, bb, &mut out, 64);
        assert_eq!(n, 3);
        assert_eq!(
            out,
            vec![11, 12, 13],
            "must read hb[offset+pos..offset+lim]"
        );
        assert_eq!(
            ctx.get_field_by_name(bb, "position").as_int(),
            Some(4),
            "position must advance to limit"
        );

        // Write path honors the offset too.
        ctx.set_field_by_name(bb, "position", Value::Int(0));
        let put = bb_write_from(&mut ctx, bb, &[0x7f, 0x7e]);
        assert_eq!(put, 2);
        assert_eq!(ctx.get_array_element(arr, 10).as_int(), Some(0x7f));
        assert_eq!(ctx.get_array_element(arr, 11).as_int(), Some(0x7e));
    }

    /// Real-JDK DirectByteBuffer shape (Reactor-Netty's default): `hb` is
    /// null and the bytes live in native memory at the `address` field.
    /// Reads and writes must go through native memory.
    #[test]
    fn bb_view_direct_named_reads_and_writes_native_memory() {
        let mut ctx = crate::test_utils::mock_ctx();
        let mut native: Vec<u8> = (0u8..32).collect();
        let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/nio/DirectByteBuffer", 8).unwrap();
        ctx.set_field_by_name(
            bb,
            "address",
            Value::Long(native.as_mut_ptr() as usize as i64),
        );
        ctx.set_field_by_name(bb, "position", Value::Int(2));
        ctx.set_field_by_name(bb, "limit", Value::Int(7));
        ctx.set_field_by_name(bb, "capacity", Value::Int(32));

        let mut out = Vec::new();
        let n = bb_read_into(&mut ctx, bb, &mut out, 64);
        assert_eq!(n, 5);
        assert_eq!(out, vec![2, 3, 4, 5, 6]);
        assert_eq!(
            ctx.get_field_by_name(bb, "position").as_int(),
            Some(7),
            "position must advance to limit"
        );

        // Write path: fill [7, 9) through the buffer.
        ctx.set_field_by_name(bb, "limit", Value::Int(32));
        let put = bb_write_from(&mut ctx, bb, &[0xAA, 0xBB]);
        assert_eq!(put, 2);
        assert_eq!(native[7], 0xAA);
        assert_eq!(native[8], 0xBB);
        assert_eq!(ctx.get_field_by_name(bb, "position").as_int(), Some(9));
    }

    /// Direct-backing accesses are clamped to capacity: a limit (or record
    /// end) past the allocation is truncated, never read out of bounds.
    #[test]
    fn bb_view_direct_clamps_to_capacity() {
        let mut ctx = crate::test_utils::mock_ctx();
        let mut native: Vec<u8> = (10u8..18).collect(); // 8 bytes
        let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/nio/DirectByteBuffer", 8).unwrap();
        ctx.set_field_by_name(
            bb,
            "address",
            Value::Long(native.as_mut_ptr() as usize as i64),
        );
        ctx.set_field_by_name(bb, "position", Value::Int(0));
        ctx.set_field_by_name(bb, "limit", Value::Int(64)); // lies past cap
        ctx.set_field_by_name(bb, "capacity", Value::Int(8));

        let v = bb_view(&mut ctx, bb);
        assert_eq!(v.lim, 8, "limit must clamp to capacity");
        let bytes = bb_bytes_range(&mut ctx, &v, 0, 100);
        assert_eq!(bytes.len(), 8, "range reads clamp to capacity");
        assert_eq!(bytes[0], 10);
        assert_eq!(bytes[7], 17);
        assert!(bb_get_byte(&mut ctx, &v, 8).is_none());
        // Writes clamp as well.
        assert_eq!(bb_put_bytes(&mut ctx, &v, 6, &[1, 2, 3, 4]), 2);
        assert_eq!(native[6], 1);
        assert_eq!(native[7], 2);
    }

    /// The pre-fix synthetic heap layout `[0]=array,[1]=pos,[2]=limit,
    /// [3]=cap` must keep resolving (synthetic-jdk mode engines).
    #[test]
    fn bb_view_synthetic_heap_slot_fallback_still_resolves() {
        let mut ctx = crate::test_utils::mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
        for i in 0..8 {
            ctx.set_array_element(arr, i, Value::Int((40 + i) as i32));
        }
        // A class OUTSIDE the mock's java/nio/*ByteBuffer named-field map,
        // so only slot-indexed reads can resolve it.
        let bb = try_alloc_concurrent_synthetic(&mut ctx, "javax/net/ssl/SyntheticBuf", 4).unwrap();
        ctx.set_field(bb, 0, Value::Object(Some(arr)));
        ctx.set_field(bb, 1, Value::Int(1)); // pos
        ctx.set_field(bb, 2, Value::Int(3)); // limit
        ctx.set_field(bb, 3, Value::Int(8)); // cap

        let mut out = Vec::new();
        let n = bb_read_into(&mut ctx, bb, &mut out, 64);
        assert_eq!(n, 2);
        assert_eq!(out, vec![41, 42]);
        assert_eq!(
            ctx.get_field(bb, 1).as_int(),
            Some(3),
            "slot-1 pos advanced"
        );
    }

    /// A heap-backed buffer whose `capacity` claims more room than its backing
    /// array actually has must not report bytes it did not move.
    ///
    /// The engine's heap arms used to copy one element at a time and let
    /// `get_array_element`/`set_array_element`'s own guards drop whatever fell
    /// past the array end — while still returning the full requested count. The
    /// read side then padded the tail with zeros and handed them to rustls as
    /// plaintext; the write side reported `data.len()` bytes produced into a
    /// buffer that had only taken some of them. Both are silent corruption in
    /// exactly the direction a caller cannot detect. The bulk intrinsics that
    /// replaced those loops bounds-check the whole range up front, so the count
    /// is now the truth. This test injects the overrun the old code hid.
    #[test]
    fn bb_heap_arms_report_only_the_bytes_that_fit() {
        let mut ctx = crate::test_utils::mock_ctx();
        // 4-byte array, but the buffer's metadata claims 16 bytes of capacity.
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        for i in 0..4 {
            ctx.set_array_element(arr, i, Value::Int((10 + i) as i32));
        }
        let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/nio/HeapByteBuffer", 8).unwrap();
        ctx.set_field_by_name(bb, "hb", Value::Object(Some(arr)));
        ctx.set_field_by_name(bb, "position", Value::Int(0));
        ctx.set_field_by_name(bb, "limit", Value::Int(16));
        ctx.set_field_by_name(bb, "capacity", Value::Int(16));
        ctx.set_field_by_name(bb, "offset", Value::Int(0));

        let mut out = Vec::new();
        let n = bb_read_into(&mut ctx, bb, &mut out, 64);
        assert_eq!(n, 4, "only the 4 real bytes exist; 16 was the buffer's lie");
        assert_eq!(out, vec![10, 11, 12, 13], "no zero padding past the array");
        assert_eq!(
            ctx.get_field_by_name(bb, "position").as_int(),
            Some(4),
            "position advances by what was actually read, not by the claim"
        );

        // Write side: 8 bytes offered into 4 bytes of real room.
        ctx.set_field_by_name(bb, "position", Value::Int(0));
        let put = bb_write_from(&mut ctx, bb, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(put, 4, "must report the 4 that landed, not all 8");
        assert_eq!(ctx.get_array_element(arr, 3).as_int(), Some(4));
        assert_eq!(
            ctx.get_field_by_name(bb, "position").as_int(),
            Some(4),
            "position advances by the bytes stored"
        );
    }

    /// A shape we cannot resolve must move zero bytes (and not panic).
    #[test]
    fn bb_view_unresolved_moves_zero_bytes() {
        let mut ctx = crate::test_utils::mock_ctx();
        let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 3).unwrap();
        let mut out = Vec::new();
        assert_eq!(bb_read_into(&mut ctx, bb, &mut out, 64), 0);
        assert!(out.is_empty());
        assert_eq!(bb_write_from(&mut ctx, bb, &[1, 2, 3]), 0);
        let v = bb_view(&mut ctx, bb);
        assert!(matches!(v.backing, BbBacking::Unresolved));
    }

    #[test]
    fn scoped_trust_roots_attach_to_one_ssl_context() {
        let ca_der = parse_cert_chain_pem(CA_CRT_PEM).unwrap()[0]
            .as_ref()
            .to_vec();
        set_selected_context_trust_roots(None);

        let ctx = fake_object_ref(1);
        let mut mock_ctx = crate::test_utils::mock_ctx();
        set_pending_tm_trust_roots(vec![ca_der.clone()]);
        attach_pending_identity_to_ctx(&mut mock_ctx, ctx, None).unwrap();

        assert!(ctx_identity(&mut mock_ctx, ctx).unwrap().is_none());
        let selected = selected_context_trust_roots().expect("context trust roots selected");
        assert_eq!(selected.root_ders, vec![ca_der]);
        let root_store = root_store_for_trust_roots(Some(&selected));
        assert_eq!(root_store.roots.len(), 1);

        let other_ctx = fake_object_ref(2);
        assert!(ctx_identity(&mut mock_ctx, other_ctx).unwrap().is_none());
        assert!(selected_context_trust_roots().is_none());
    }

    #[test]
    fn legacy_extra_trust_registration_does_not_expand_global_roots() {
        let ca_der = parse_cert_chain_pem(CA_CRT_PEM).unwrap()[0]
            .as_ref()
            .to_vec();
        set_selected_context_trust_roots(None);

        add_extra_trust_root_der(ca_der);

        assert!(selected_context_trust_roots().is_none());
        assert!(trust_roots_pem(None).is_empty());
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
        // The cipher substring was never asserted here, which is why the
        // rustls spelling survived in this string. It is now the ONLY
        // behavioural exercise of `suite_to_java_cipher_name` that needs no
        // network: a real in-process TLS 1.3 handshake, whose name must be
        // the one HotSpot reports. Measured on this host, HotSpot 25.0.3+9-LTS
        // answers `TLS_AES_256_GCM_SHA384` for a live TLS 1.3 session and lists
        // ZERO supported suites beginning `TLS13_`.
        assert!(
            msg.contains("cipher=TLS_"),
            "cipher must carry JSSE's spelling: {}",
            msg
        );
        assert!(
            !msg.contains("cipher=TLS13_"),
            "cipher is still in rustls's TLS 1.3 spelling, which JSSE never \
             produces -- `suite_to_java_cipher_name` was bypassed: {}",
            msg
        );
    }

    /// The family guard for the rustls-vs-JSSE cipher spelling.
    ///
    /// The helper `http_url_connection::jsse_cipher_suite_name` existed, was
    /// unit-tested in both directions, and had exactly ONE of the eight
    /// producers of a rustls suite name as a caller — the other seven lived in
    /// this file and each re-spelled the name by hand. Testing the FUNCTION is
    /// what let that happen: nothing asserted that anyone CALLS it. This
    /// asserts the call, by shape, over the working tree.
    ///
    /// ZERO occurrences of the raw `{:?}`-on-a-suite idiom may exist in this
    /// file. The translation is `suite_to_java_cipher_name`, which takes a
    /// `rustls::CipherSuite` rather than a `Debug` string, so every producer —
    /// including `engine_take_pending_trust_check`, whose value never reaches
    /// Java — goes through it and the old two-exception carve-out is gone. A
    /// new producer, or a rewrite of an adapted call site back to the raw
    /// idiom, fails here.
    ///
    /// Reads the working tree rather than `include_str!` — this repository is
    /// edited from both Windows and Linux, so `\r` is normalised.
    #[test]
    fn the_only_rustls_suite_spelling_left_is_the_adapters_own() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("t27_tls.rs");
        let Ok(src) = std::fs::read_to_string(&path) else {
            println!("t27_tls.rs not on disk at {path:?}; witness skipped");
            return;
        };
        let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();

        // Assembled from fragments so this test's own source does not contain
        // the needle it searches for.
        let needle = format!("{}{}{}", "format!(\"{:", "?}\", cs.", "suite())");

        let mut found: Vec<String> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            // Prose mentions the idiom in doc comments; only code counts.
            if t.starts_with("//") {
                continue;
            }
            if !line.contains(&needle) {
                continue;
            }
            let owner = lines[..=i]
                .iter()
                .rev()
                .find_map(|l| {
                    let l = l.trim_start();
                    for p in ["pub(crate) fn ", "pub fn ", "fn "] {
                        if let Some(rest) = l.strip_prefix(p) {
                            return Some(
                                rest.split(['(', '<', ' ']).next().unwrap_or("").to_string(),
                            );
                        }
                    }
                    None
                })
                .unwrap_or_else(|| format!("<no enclosing fn, line {}>", i + 1));
            found.push(owner);
        }
        found.sort();
        found.dedup();

        // Was two (`negotiated_suite_name`, the adapter, and
        // `engine_take_pending_trust_check`, whose value never reaches Java).
        // Both are now on the typed `suite_to_java_cipher_name`, which takes a
        // `rustls::CipherSuite` and cannot be handed a `Debug` string at all —
        // so the raw idiom must appear NOWHERE in this file. A producer that
        // re-introduces it fails here, exactly as before, on a stricter rule.
        let expected: Vec<String> = Vec::new();
        assert_eq!(
            found, expected,
            "the raw rustls suite spelling must appear NOWHERE in this file. \
             Found it in: {found:?}. Every producer feeds \
             `SSLSession.getCipherSuite()`, which is contracted to return the \
             IANA/JSSE name -- HotSpot 25 lists ZERO suites spelled `TLS13_`. \
             Use `suite_to_java_cipher_name`, which takes the suite itself."
        );
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
                    stream
                        .conn
                        .read_tls(&mut EintrIo::new(&mut stream.sock))
                        .map_err(|e| e.to_string())?;
                    stream
                        .conn
                        .process_new_packets()
                        .map_err(|e| e.to_string())?;
                }
                if stream.conn.wants_write() {
                    stream
                        .conn
                        .write_tls(&mut EintrIo::new(&mut stream.sock))
                        .map_err(|e| e.to_string())?;
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
        let client_config =
            build_client_config(roots, &["h2"], Some((CLIENT_CRT_PEM, CLIENT_KEY_PEM))).unwrap();
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let sni = ServerName::try_from("localhost".to_string()).unwrap();
        let conn = ClientConnection::new(client_config, sni).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
            }
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
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
                        stream
                            .conn
                            .read_tls(&mut EintrIo::new(&mut stream.sock))
                            .map_err(|e| e.to_string())?;
                        stream
                            .conn
                            .process_new_packets()
                            .map_err(|e| e.to_string())?;
                    }
                    if stream.conn.wants_write() {
                        stream
                            .conn
                            .write_tls(&mut EintrIo::new(&mut stream.sock))
                            .map_err(|e| e.to_string())?;
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
                    stream
                        .conn
                        .write_tls(&mut EintrIo::new(&mut stream.sock))
                        .unwrap();
                }
                if stream.conn.wants_read() {
                    stream
                        .conn
                        .read_tls(&mut EintrIo::new(&mut stream.sock))
                        .unwrap();
                    stream.conn.process_new_packets().unwrap();
                }
            }
            let certs = stream.conn.peer_certificates().expect("peer cert present");
            let leaf = &certs[0];
            // Cheap contains-check against the embedded CN byte substring:
            // server1 cert has `CN=foo.test`, server2 has `CN=bar.test`.
            let bytes = leaf.as_ref();
            let needle = expect_cn.as_bytes();
            let found = bytes.windows(needle.len()).any(|w| w == needle);
            assert!(
                found,
                "SNI {} did not get leaf containing {}",
                sni_name, expect_cn
            );
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
        let config = build_client_config(roots, &["http/1.1"], None).expect("client config");
        let tcp = TcpStream::connect("www.google.com:443").expect("TCP connect");
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .ok();
        tcp.set_write_timeout(Some(std::time::Duration::from_secs(10)))
            .ok();
        let sni = ServerName::try_from("www.google.com".to_string()).unwrap();
        let conn = ClientConnection::new(config, sni).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        // Drive handshake.
        while stream.conn.is_handshaking() {
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
            }
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
                stream.conn.process_new_packets().unwrap();
            }
        }
        // Verify negotiated protocol is TLS 1.2 or 1.3.
        let proto = stream.conn.protocol_version().expect("protocol version");
        assert!(
            proto == rustls::ProtocolVersion::TLSv1_3 || proto == rustls::ProtocolVersion::TLSv1_2,
            "unexpected protocol: {:?}",
            proto,
        );
        // Send a minimal HTTP/1.1 GET.
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: www.google.com\r\nConnection: close\r\n\r\n")
            .unwrap();
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
    /// server must resume, demonstrating that rustls's TLS 1.3 ticket cache is
    /// retained by the shared client configuration.
    #[test]
    fn t27_session_resumption() {
        let server_config =
            build_server_config_single_cert(SERVER_CRT_PEM, SERVER_KEY_PEM, &[], false, None)
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
                        stream
                            .conn
                            .read_tls(&mut EintrIo::new(&mut stream.sock))
                            .map_err(|e| e.to_string())?;
                        stream
                            .conn
                            .process_new_packets()
                            .map_err(|e| e.to_string())?;
                    }
                    if stream.conn.wants_write() {
                        stream
                            .conn
                            .write_tls(&mut EintrIo::new(&mut stream.sock))
                            .map_err(|e| e.to_string())?;
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
                if stream.conn.wants_write() {
                    stream
                        .conn
                        .write_tls(&mut EintrIo::new(&mut stream.sock))
                        .unwrap();
                }
                if stream.conn.wants_read() {
                    stream
                        .conn
                        .read_tls(&mut EintrIo::new(&mut stream.sock))
                        .unwrap();
                    stream.conn.process_new_packets().unwrap();
                }
            }
            assert!(
                stream.conn.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3),
                "first handshake should be TLSv1.3"
            );
            // The server sends TLS 1.3's NewSessionTicket after the handshake.
            // Drain through its close_notify so rustls stores that ticket on
            // the shared ClientConfig before the next connection is created.
            let mut eof = [0u8; 1];
            assert_eq!(
                stream.read(&mut eof).unwrap(),
                0,
                "first connection should close after delivering its ticket"
            );
            stream.conn.send_close_notify();
            let _ = stream.flush();
        }
        // Second connection must use the ticket received on the first.
        {
            let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let sni = ServerName::try_from("localhost".to_string()).unwrap();
            let conn = ClientConnection::new(config.clone(), sni).unwrap();
            let mut stream = StreamOwned::new(conn, tcp);
            while stream.conn.is_handshaking() {
                if stream.conn.wants_write() {
                    stream
                        .conn
                        .write_tls(&mut EintrIo::new(&mut stream.sock))
                        .unwrap();
                }
                if stream.conn.wants_read() {
                    stream
                        .conn
                        .read_tls(&mut EintrIo::new(&mut stream.sock))
                        .unwrap();
                    stream.conn.process_new_packets().unwrap();
                }
            }
            assert!(
                stream.conn.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3),
                "second (resumed) handshake should be TLSv1.3"
            );
            assert_eq!(
                stream.conn.handshake_kind(),
                Some(rustls::HandshakeKind::Resumed),
                "second handshake must use the ticket from the first connection"
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
        // JSSE's delegated-task contract. `getDelegatedTask` was unregistered
        // until 2026-08-16, so a caller that followed the NEED_TASK this
        // engine can report had no way to satisfy it. See `DelegatedTask`.
        assert!(r
            .find(cls, "getDelegatedTask", "()Ljava/lang/Runnable;")
            .is_some());
        assert!(r.find("java/lang/Runnable", "run", "()V").is_some());
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
            .find(cls, "getApplicationProtocol", "()Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "setApplicationProtocols", "([Ljava/lang/String;)V")
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

        // TC0622: the SSLSession accessors must also be registered in the
        // real-mode path (they previously lived only in the synthetic-jdk-gated
        // register_p68_ssl, so SSLEngine.getSession().getApplicationBufferSize()
        // threw AbstractMethodError and killed Tomcat's NioEndpoint processor).
        let sess = "javax/net/ssl/SSLSession";
        assert!(r.find(sess, "getApplicationBufferSize", "()I").is_some());
        assert!(r.find(sess, "getPacketBufferSize", "()I").is_some());
        assert!(r
            .find(sess, "getProtocol", "()Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(sess, "getCipherSuite", "()Ljava/lang/String;")
            .is_some());
        assert!(r.find(sess, "isValid", "()Z").is_some());
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
    fn a_delegated_task_is_owed_once_and_handed_over_once() {
        // The state machine behind JSSE's NEED_TASK contract. See
        // `DelegatedTask` for what each transition is load-bearing for.
        let id = super::engine_alloc_id();
        super::engine_registry()
            .write()
            .insert(id, super::EngineState::default());

        // First need: deferred, and a Runnable is now owed.
        assert_eq!(super::engine_begin_or_defer(id), Ok(true));
        super::with_engine(id, |s| {
            assert_eq!(s.delegated_task, super::DelegatedTask::Owed);
            assert!(s.task_unclaimed);
        });

        // Collected exactly once — netty's in-line `runDelegatedTasks` loop
        // terminates on the second, null answer and would otherwise spin.
        assert!(super::claim_delegated_task(id));
        assert!(!super::claim_delegated_task(id));
        super::with_engine(id, |s| {
            assert_eq!(s.delegated_task, super::DelegatedTask::HandedOut)
        });

        // Handed out and not yet run: the engine makes no progress, however
        // many times it is asked. This refusal is the whole feature — it is
        // what `test{Client,Server}HandshakeTimeoutBecauseExecutorNotExecute`
        // measure, and it has to survive a retry loop.
        assert_eq!(super::engine_begin_or_defer(id), Ok(true));
        assert_eq!(super::engine_begin_or_defer(id), Ok(true));

        super::engine_registry().write().remove(&id);
    }

    #[test]
    fn a_task_done_inline_is_still_handed_over_when_the_caller_asks() {
        // Rule 2 on `DelegatedTask`: netty's `SslTasksRunner.run()` returns
        // WITHOUT calling `runComplete()` when `getDelegatedTask()` answers
        // null, so `SslHandler` stays in STATE_PROCESS_TASK — where `decode()`
        // and `flush()` are both no-ops — and the connection is wedged for
        // good. Measured: `getDelegatedTask id=3 hand_out=false` on the
        // executor thread, immediately after the event-loop thread had done
        // the work inline, was `testHandshakeWithExecutorJDK`'s failure.
        //
        // A promise made must therefore be answered even when the work was
        // meanwhile done by somebody else.
        let id = super::engine_alloc_id();
        let mut st = super::EngineState::default();
        // Exactly what the `Owed` arm of `engine_begin_or_defer` leaves
        // behind when it self-heals: the work is done, the promise is not.
        st.delegated_task = super::DelegatedTask::None;
        st.delegated_task_armed = true;
        st.task_unclaimed = true;
        super::engine_registry().write().insert(id, st);

        assert!(
            super::claim_delegated_task(id),
            "a caller told NEED_TASK must never be answered with null"
        );
        assert!(!super::claim_delegated_task(id));

        super::engine_registry().write().remove(&id);
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
        assert!(
            cfg.is_ok(),
            "expected Ok with identity installed: {:?}",
            cfg.err()
        );
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

    /// `ConnCheckout` must put the connection back on EVERY exit, including the
    /// early `return Err(...)` the record loop takes on a `process_new_packets`
    /// failure. A missed restore does not fail a test — it leaves `conn == None`
    /// for the life of that engine, so every later `wrap`/`unwrap` silently does
    /// nothing and the connection just stops.
    #[test]
    fn a_checked_out_connection_is_restored_on_every_exit() {
        let id = super::engine_alloc_id();
        {
            let mut st = super::EngineState::default();
            st.is_client = true;
            st.peer_host = Some("localhost".to_string());
            st.client_config = Some(
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
            super::engine_begin(&mut st).expect("begin");
            assert!(st.conn.is_some());
            super::engine_registry().write().insert(id, st);
        }

        // Normal scope exit.
        {
            let checkout = super::ConnCheckout::take(id);
            assert!(checkout.conn.is_some(), "the connection must come out");
            assert!(
                super::with_engine(id, |s| s.conn.is_none() && s.conn_checked_out).unwrap(),
                "while on loan the engine must report `conn_checked_out`, not just an absent conn"
            );
            // `engine_begin` must refuse to build a rival connection in this
            // window — the restore below would silently discard it.
            super::with_engine(id, |s| {
                super::engine_begin(s).expect("begin during checkout");
                assert!(
                    s.conn.is_none(),
                    "engine_begin built a second connection while one was on loan"
                );
            });
        }
        assert!(
            super::with_engine(id, |s| s.conn.is_some() && !s.conn_checked_out).unwrap(),
            "the connection must be back after a normal scope exit"
        );

        // Early-return exit, the shape the record loop's `return Err(...)` takes.
        fn bails_out(id: i32) -> Result<(), ()> {
            let _checkout = super::ConnCheckout::take(id);
            Err(())
        }
        assert!(bails_out(id).is_err());
        assert!(
            super::with_engine(id, |s| s.conn.is_some() && !s.conn_checked_out).unwrap(),
            "the connection must be back after an early return"
        );

        // A `?`-style exit out of a nested scope, and a panic-driven unwind.
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _checkout = super::ConnCheckout::take(id);
            panic!("simulated fault inside the record loop");
        }));
        assert!(unwound.is_err());
        assert!(
            super::with_engine(id, |s| s.conn.is_some() && !s.conn_checked_out).unwrap(),
            "the connection must be back after an unwind"
        );

        super::engine_registry().write().remove(&id);
    }

    /// A `ServerCertVerifier` that refuses every chain, the way a Java
    /// `X509TrustManager` throwing `CertificateException` would if its verdict
    /// reached rustls at verification time instead of after the handshake.
    #[derive(Debug)]
    struct AlwaysRejectVerifier {
        algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
        /// `false` makes this the ACCEPTING control arm — the verdict the
        /// deferred design effectively gives rustls today (it always accepts at
        /// verification time and consults Java afterwards). The control is what
        /// keeps the assertions below from being vacuous.
        reject: bool,
    }

    impl rustls::client::danger::ServerCertVerifier for AlwaysRejectVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            if !self.reject {
                return Ok(rustls::client::danger::ServerCertVerified::assertion());
            }
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature_lenient(message, cert, dss, &self.algorithms)
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature_lenient(message, cert, dss, &self.algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.algorithms.supported_schemes()
        }
    }

    /// **The destination for the deferred-trust-check refactor, proven before the
    /// refactor.**
    ///
    /// `testHandshakeFailureOnlyFireExceptionOnce` (`SslHandlerTest:1546`) asserts
    /// the SERVER's handshake future fails when the CLIENT's `TrustManager`
    /// rejects the chain. Today it cannot: the trust check is armed by
    /// `engine_take_pending_trust_check`, which fires only once
    /// `!conn.is_handshaking()` -- by construction AFTER the client's `Finished`
    /// has gone out. The server therefore completes a valid TLS 1.3 handshake,
    /// netty runs `setHandshakeSuccess()`, and the alert arriving a moment later
    /// cannot fail an already-completed promise.
    ///
    /// This asserts the property the current design cannot deliver and a
    /// verifier-time verdict can: with the rejection raised INSIDE
    /// `verify_server_cert`, the server receives an alert it can DECRYPT while
    /// `is_handshaking()` is still true -- i.e. under handshake keys, not under
    /// the application keys the cheap shortcut would need. (That shortcut was
    /// measured and does not work: discarding rustls's queued flight before
    /// queueing the alert desynchronises the TLS 1.3 key schedule and the server
    /// reads `DecryptError` instead of the alert.)
    ///
    /// If this ever starts failing, section B of
    /// the openssl-key-material-and-engine-residuals write-up (now retired)
    /// has lost its destination and the plan needs rethinking before any more of
    /// it is built.
    /// Drive a client/server `EngineState` pair whose client verifier either
    /// rejects or accepts, and report what the SERVER observed:
    /// `(saw_error_while_handshaking, error_text, still_handshaking_at_end)`.
    fn drive_pair_with_client_verifier(reject: bool) -> (bool, String, bool) {
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let verifier: std::sync::Arc<dyn rustls::client::danger::ServerCertVerifier> =
            std::sync::Arc::new(AlwaysRejectVerifier {
                algorithms: provider.signature_verification_algorithms.clone(),
                reject,
            });
        let mut client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"h2".to_vec()];

        let mut client = super::EngineState::default();
        client.is_client = true;
        client.peer_host = Some("localhost".to_string());
        client.alpn_protocols = vec![b"h2".to_vec()];
        client.client_config = Some(std::sync::Arc::new(client_config));

        let mut server = super::EngineState::default();
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

        // The client's rejection surfaces out of its own `unwrap` of the server
        // flight; what matters is what the SERVER can then read.
        let mut server_saw_alert_while_handshaking = false;
        let mut server_error: Option<String> = None;
        for _ in 0..32 {
            let _ = super::engine_wrap_pump(&mut client, &[], 65536);
            let to_server = std::mem::take(&mut client.outbound);
            if !to_server.is_empty() {
                let was_handshaking = server
                    .conn
                    .as_ref()
                    .map(|c| c.is_handshaking())
                    .unwrap_or(false);
                match super::engine_unwrap_pump(&mut server, &to_server) {
                    Ok(_) => {}
                    Err(e) => {
                        // The alert decrypted and rustls reported it. That is the
                        // whole property: it arrived under handshake keys, so the
                        // server learns of the failure before it could complete.
                        if was_handshaking {
                            server_saw_alert_while_handshaking = true;
                        }
                        server_error = Some(format!("{e:?}"));
                        break;
                    }
                }
            }
            let _ = super::engine_wrap_pump(&mut server, &[], 65536);
            let to_client = std::mem::take(&mut server.outbound);
            if !to_client.is_empty() {
                // The client's own unwrap is where its verifier runs and where it
                // raises; that error is not what this test is about.
                let _ = super::engine_unwrap_pump(&mut client, &to_client);
            }
        }

        let still_handshaking = server
            .conn
            .as_ref()
            .map(|c| c.is_handshaking())
            .unwrap_or(false);
        (
            server_saw_alert_while_handshaking,
            server_error.unwrap_or_default(),
            still_handshaking,
        )
    }

    #[test]
    fn a_verifier_time_rejection_reaches_the_server_while_it_is_still_handshaking() {
        // CONTROL first: the same driver with an ACCEPTING verifier -- which is
        // what the deferred design gives rustls today -- must NOT produce any of
        // the three signals. Without this arm the assertions below would pass on
        // a driver that simply never completed a handshake at all.
        let (accept_saw, accept_err, accept_handshaking) = drive_pair_with_client_verifier(false);
        assert!(
            !accept_saw,
            "control: an accepted chain must not make the server see an error, got {accept_err}"
        );
        assert!(
            !accept_handshaking,
            "control: with the chain accepted the server must COMPLETE its handshake -- \
             if it does not, the driver is broken and the reject arm proves nothing"
        );

        let (saw, err, still_handshaking) = drive_pair_with_client_verifier(true);
        assert!(
            saw,
            "the server must see the client rejection while still handshaking; server_error={err}"
        );
        // And it must NOT be a decrypt failure: a `DecryptError` here would mean
        // the alert went out under the wrong keys, which is exactly what the
        // discard-the-queued-flight shortcut produced.
        assert!(
            !err.contains("DecryptError"),
            "the alert must be decryptable under handshake keys, got {err}"
        );
        assert!(
            still_handshaking,
            "the server must still be handshaking, not completed"
        );
    }

    /// Drive a client/server `EngineState` pair through a complete loopback
    /// handshake (same shape as `wp51_loopback_handshake_via_engine_state`) and
    /// return them, with the client's captured peer chain populated.
    fn handshaked_pair() -> (super::EngineState, super::EngineState) {
        let mut client = super::EngineState::default();
        let mut server = super::EngineState::default();
        client.is_client = true;
        client.peer_host = Some("localhost".to_string());
        client.client_config = Some(
            super::build_client_config(
                {
                    let mut roots = RootCertStore::empty();
                    for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
                        roots.add(c).unwrap();
                    }
                    roots
                },
                &[],
                None,
            )
            .unwrap(),
        );
        server.is_client = false;
        server.server_config = Some(
            super::build_server_config_single_cert(
                SERVER_CRT_PEM,
                SERVER_KEY_PEM,
                &[],
                false,
                None,
            )
            .unwrap(),
        );
        super::engine_begin(&mut client).expect("client begin");
        super::engine_begin(&mut server).expect("server begin");
        for _ in 0..32 {
            let _ = super::engine_wrap_pump(&mut client, &[], 65536);
            let buf = std::mem::take(&mut client.outbound);
            if !buf.is_empty() {
                let _ = super::engine_unwrap_pump(&mut server, &buf);
            }
            let _ = super::engine_wrap_pump(&mut server, &[], 65536);
            let buf2 = std::mem::take(&mut server.outbound);
            if !buf2.is_empty() {
                let _ = super::engine_unwrap_pump(&mut client, &buf2);
            }
            super::engine_capture_negotiation(&mut client);
            super::engine_capture_negotiation(&mut server);
            let done = |s: &super::EngineState| {
                s.conn
                    .as_ref()
                    .map(|c| !c.is_handshaking())
                    .unwrap_or(false)
            };
            if done(&client) && done(&server) {
                break;
            }
        }
        (client, server)
    }

    #[test]
    fn endpoint_alg_only_https_and_ldaps_verify_identity() {
        assert!(super::endpoint_alg_verifies_identity("HTTPS"));
        assert!(super::endpoint_alg_verifies_identity("https"));
        assert!(super::endpoint_alg_verifies_identity("LDAPS"));
        // Not an identification algorithm JSSE knows: treat as "no check"
        // rather than inventing one, so we never reject what JSSE accepts.
        assert!(!super::endpoint_alg_verifies_identity(""));
        assert!(!super::endpoint_alg_verifies_identity("NONE"));
    }

    /// The engine id these trust-check tests pass through.
    ///
    /// `handshaked_pair()` builds `EngineState`s directly instead of
    /// registering engines, so there is no registry id to quote — and
    /// `engine_take_pending_trust_check` only copies the value into
    /// `PendingTrustCheck::engine_id`, so any stable value serves.
    const TEST_ENGINE_ID: i32 = 0;

    /// REGRESSION (`TestSecurity2018.testCVE_2018_8034`): a client engine
    /// configured with `setEndpointIdentificationAlgorithm("HTTPS")` must
    /// refuse a certificate that does not name the host it dialled — even
    /// though the chain itself is perfectly trusted and even though the
    /// application's own `TrustManager` accepts everything.
    #[test]
    fn endpoint_identity_is_pending_and_rejects_a_mismatched_host() {
        let (mut client, _server) = handshaked_pair();
        assert!(
            !client.peer_cert_chain_der.is_empty(),
            "client must have captured the server chain"
        );

        // 1. No algorithm configured and no TrustManager: nothing pending —
        //    exactly as cheap as before this fix.
        client.trust_check_done = false;
        client.endpoint_id_alg = None;
        assert!(super::engine_take_pending_trust_check(TEST_ENGINE_ID, &mut client).is_none());

        // 2. HTTPS configured: a pending check appears even with no
        //    TrustManager attached, because JSSE's own default manager is what
        //    performs identification.
        client.trust_check_done = false;
        client.endpoint_id_alg = Some("HTTPS".to_string());
        client.peer_host = Some("localhost".to_string());
        let pending = super::engine_take_pending_trust_check(TEST_ENGINE_ID, &mut client)
            .expect("identity check pending");
        assert!(pending.trust_ctx_key.is_none());
        // The engine id is carried through so the deferred half
        // (`engine_run_trust_check`, which runs after the registry lock is
        // dropped) can find its engine again. Nothing asserted it, which is
        // why adding the parameter broke three call sites and no test.
        assert_eq!(pending.engine_id, TEST_ENGINE_ID);
        assert_eq!(
            pending.endpoint_identity,
            Some(("HTTPS".to_string(), "localhost".to_string()))
        );
        // The host the engine actually dialled matches the leaf: accepted.
        assert!(
            crate::x509_manager::check_endpoint_identity(&pending.peer_chain_der, "localhost")
                .is_ok()
        );

        // 3. The CVE shape: same trusted chain, different host. The in-tree
        //    test leaf names localhost/foo.test/bar.test/127.0.0.1, so a host
        //    outside that set must be refused.
        assert!(
            crate::x509_manager::check_endpoint_identity(&pending.peer_chain_der, "evil.test")
                .is_err(),
            "a certificate that does not name the dialled host must be refused"
        );

        // 4. A server engine never runs client-side identification, whatever
        //    is configured on it.
        client.trust_check_done = false;
        client.is_client = false;
        let pending = super::engine_take_pending_trust_check(TEST_ENGINE_ID, &mut client);
        assert!(
            pending.is_none(),
            "server engines do not identify endpoints"
        );
    }

    /// REGRESSION (`NettyReactiveWebServerFactoryTests.whenSslBundleIsUpdatedThenSslIsReloaded`):
    /// which TrustManager is in force decides WHO identifies the endpoint, and
    /// getting that wrong rejects a connection a real JDK accepts.
    ///
    /// The three cases are the three branches of `SSLContextImpl.chooseTrustManager`,
    /// and they must not collapse into each other: an empty array is JSSE's own
    /// default manager (identifies), a plain `X509TrustManager` is wrapped by
    /// `AbstractTrustManagerWrapper` (JSSE identifies AFTER it — the
    /// CVE-2018-8034 case Tomcat's `TesterSupport.TrustAllCerts` exercises),
    /// and an `X509ExtendedTrustManager` is used as-is (JSSE adds nothing, and
    /// Netty's `X509TrustManagerWrapper` is one).
    #[test]
    fn an_extended_trust_manager_owns_endpoint_identification() {
        let mut ctx = crate::test_utils::mock_ctx();
        let extended = ctx
            .ensure_class_initialized("javax/net/ssl/X509ExtendedTrustManager")
            .expect("mock class");
        let tm_iface = ctx
            .ensure_class_initialized("javax/net/ssl/X509TrustManager")
            .expect("mock class");
        ctx.set_superclass(extended, tm_iface);

        // 1. No application TrustManager: JSSE's own default identifies.
        assert!(
            super::jsse_owns_endpoint_identification(&mut ctx, &[]),
            "with no application manager, JSSE's default X509TrustManagerImpl identifies"
        );

        // 2. A PLAIN X509TrustManager — the accept-everything test shape.
        //    JSSE wraps it and still identifies, so this must stay strict or
        //    `TestSecurity2018.testCVE_2018_8034` regresses.
        let plain_cls = ctx
            .ensure_class_initialized("org/example/TrustAllCerts")
            .expect("mock class");
        let plain = ctx.alloc_object(plain_cls, 1);
        assert!(
            super::jsse_owns_endpoint_identification(&mut ctx, &[plain]),
            "a plain X509TrustManager is wrapped by JSSE, which then identifies"
        );

        // 3. An X509ExtendedTrustManager subclass — Netty's
        //    `X509TrustManagerWrapper`. JSSE uses it as-is and adds no check.
        let netty_cls = ctx
            .ensure_class_initialized("io/netty/handler/ssl/util/X509TrustManagerWrapper")
            .expect("mock class");
        ctx.set_superclass(netty_cls, extended);
        let netty = ctx.alloc_object(netty_cls, 1);
        assert!(
            !super::jsse_owns_endpoint_identification(&mut ctx, &[netty]),
            "an X509ExtendedTrustManager owns identification; JSSE adds no check of its own"
        );

        // 3b. The class itself, not only a subclass.
        let direct = ctx.alloc_object(extended, 1);
        assert!(!super::jsse_owns_endpoint_identification(
            &mut ctx,
            &[direct]
        ));

        // 4. `chooseTrustManager` takes the FIRST manager, so a plain one ahead
        //    of an extended one still means JSSE identifies.
        let plain2 = ctx.alloc_object(plain_cls, 1);
        assert!(
            super::jsse_owns_endpoint_identification(&mut ctx, &[plain2, netty]),
            "the FIRST manager decides, matching SSLContextImpl.chooseTrustManager"
        );

        // 5. JSSE's OWN X509TrustManagerImpl is an X509ExtendedTrustManager
        //    too, but its `checkServerTrusted` is served natively here and
        //    performs no identification — so this VM must, or nobody does
        //    (`testClientHostnameValidationFail`). This is the case
        //    `SslContextBuilder.trustManager(File)` produces.
        let jsse_cls = ctx
            .ensure_class_initialized("sun/security/ssl/X509TrustManagerImpl")
            .expect("mock class");
        ctx.set_superclass(jsse_cls, extended);
        let jsse = ctx.alloc_object(jsse_cls, 1);
        assert!(
            super::jsse_owns_endpoint_identification(&mut ctx, &[jsse]),
            "JSSE's own trust manager does not identify on this VM, so this VM must"
        );
    }

    #[test]
    fn wrap_consumes_no_app_data_before_finished_is_reported() {
        // REGRESSION (websocket-jsse-ssl-bytes-consumed-during-write): rustls
        // reports `is_handshaking() == false` one flight BEFORE the engine has
        // told the caller the handshake ended. `handshake_status_of` keeps
        // answering NEED_WRAP through that window on purpose (the TLS 1.2
        // server-flight fix), so a caller that correctly obeys NEED_WRAP calls
        // `wrap(src, dst)` while, from its point of view, it is still
        // handshaking -- and JSSE's contract says such a wrap consumes NOTHING
        // from `src`.
        //
        // Gating app-data consumption on `!is_handshaking()` alone made the
        // engine treat `src` as application data in that window. Tomcat's
        // WebSocket client passes a 16921-byte STATIC
        // `AsyncChannelWrapperSecure.DUMMY`, so the engine drained 16384 bytes
        // of zeros out of it, ENCRYPTED them onto the wire mid-upgrade, and
        // reported bytesConsumed=16384 -- tripping
        // `AsyncChannelWrapperSecure.checkResult`'s "Bytes were consumed from
        // the input during a write" and killing every wss:// connect. DUMMY is
        // never rewound, so the damage leaked into later connections too.
        let mut client = super::EngineState::default();
        let mut server = super::EngineState::default();
        client.is_client = true;
        client.peer_host = Some("localhost".to_string());
        client.client_config = Some(
            super::build_client_config(
                {
                    let mut roots = RootCertStore::empty();
                    for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
                        roots.add(c).unwrap();
                    }
                    roots
                },
                &[],
                None,
            )
            .unwrap(),
        );
        server.is_client = false;
        server.server_config = Some(
            super::build_server_config_single_cert(
                SERVER_CRT_PEM,
                SERVER_KEY_PEM,
                &[],
                false,
                None,
            )
            .unwrap(),
        );
        super::engine_begin(&mut client).expect("client begin");
        super::engine_begin(&mut server).expect("server begin");

        for _ in 0..32 {
            let _ = super::engine_wrap_pump(&mut client, &[], 65536);
            let buf = std::mem::take(&mut client.outbound);
            if !buf.is_empty() {
                let _ = super::engine_unwrap_pump(&mut server, &buf);
            }
            let _ = super::engine_wrap_pump(&mut server, &[], 65536);
            let buf2 = std::mem::take(&mut server.outbound);
            if !buf2.is_empty() {
                let _ = super::engine_unwrap_pump(&mut client, &buf2);
            }
            if !client.conn.as_ref().unwrap().is_handshaking() {
                break;
            }
        }

        // The exact window: rustls is done, the caller has NOT been told.
        // Nothing in the pumps sets `handshake_finished_reported`; only
        // `do_wrap`/`do_unwrap` do, when they hand a FINISHED result back.
        assert!(
            !client.conn.as_ref().unwrap().is_handshaking(),
            "client handshake did not complete"
        );
        assert!(
            !client.handshake_finished_reported,
            "precondition: FINISHED has not been reported to the caller yet"
        );

        let dummy = vec![0u8; 16921];
        let (consumed, _) = super::engine_wrap_pump(&mut client, &dummy, 65536);
        assert_eq!(
            consumed, 0,
            "wrap() consumed the caller's buffer before reporting FINISHED; \
             Tomcat's AsyncChannelWrapperSecure asserts bytesConsumed == 0 for \
             every handshake-time wrap"
        );

        // Once FINISHED has been reported, an ordinary application write must
        // still work -- the fix must not wedge the post-handshake path.
        client.handshake_finished_reported = true;
        let (consumed_after, _) = super::engine_wrap_pump(&mut client, &dummy, 65536);
        assert!(
            consumed_after > 0,
            "application data must be consumed once the handshake is reported finished"
        );
    }

    // -----------------------------------------------------------------------
    // E12/E22 — the "nothing was negotiated" session, in the LIVE registrar
    // -----------------------------------------------------------------------
    //
    // `phases_late::ssl_security` already has tests of this shape. They pass,
    // and they measured code that is DEAD: `--dump-native-registry` shows THIS
    // file's `getProtocol`/`getCipherSuite`/`getId`/`isValid` owning the
    // `javax/net/ssl/SSLSession` slots in the default real-JDK mode, which is
    // the mode `--jdk-only` runs. Duplicating the coverage here is the point:
    // a green test over an overwritten registration is worse than no test,
    // because it reads as proof.

    fn session_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        super::register_ssl_session_real(&mut r);
        r
    }

    /// DOOR 1 of `regression-suite/src/RSslNullSession.java`, without a VM:
    /// the session `ssl_security::new13_alloc_null_ssl_session` mints for an
    /// `SSLSocket` that was never connected (`tls_id = -1`).
    ///
    /// `getId` and `isValid` are the two this file decides and the two the
    /// nominated patch would have missed — a field-count-only test treats
    /// every shape narrower than 7 as "negotiated", which is exactly this one.
    ///
    /// E42 — **run at BOTH widths, and 4 is the one that ships.**
    /// `NEW13_SSL_SESS_FIELDS` went 3 -> 4 so `putValue` would stop writing
    /// over slot 2, which moved the live null session onto
    /// `session_has_negotiated`'s `_ => true` arm. Width 3 stays here because
    /// the merged `3 | 4` arm still answers it defensively and a regression
    /// dropping either half would otherwise be invisible from this crate;
    /// `ssl_security::new13_tests` holds the same line from the other side.
    #[test]
    fn the_null_socket_session_has_no_id_and_is_not_valid() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();

        let get_id = r
            .find("javax/net/ssl/SSLSession", "getId", "()[B")
            .expect("getId registered");
        let is_valid = r
            .find("javax/net/ssl/SSLSession", "isValid", "()Z")
            .expect("isValid registered");

        // [0]=proto [1]=cipher [2]=tls_id, tls_id = -1 => never connected.
        // Width 4 adds [3]=attrs, which is the whole reason it is 4.
        for width in [3usize, 4] {
            let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), width);
            let p = ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL);
            let c = ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE);
            ctx.set_field(sess, 0, Value::Object(Some(p)));
            ctx.set_field(sess, 1, Value::Object(Some(c)));
            ctx.set_field(sess, 2, Value::Int(-1));
            if width > 3 {
                ctx.set_field(sess, 3, Value::Object(None));
            }
            let this = &[Value::Object(Some(sess))];

            match get_id(&mut ctx, this) {
                Ok(Some(Value::Object(Some(a)))) => assert_eq!(
                    ctx.array_length(a),
                    0,
                    "width {width}: HotSpot answers byte[0] for a session that \
                     negotiated nothing; Tomcat's JSSESupport.java:171 tests \
                     `length == 0` exactly"
                ),
                other => panic!("getId must return an array, got {other:?}"),
            }

            assert_eq!(
                is_valid(&mut ctx, this).unwrap(),
                Some(Value::Int(0)),
                "width {width}: HotSpot 25.0.3+9-LTS, unconnected SSLSocket: \
                 isValid() = false"
            );
        }
    }

    /// MUTATION CHECK for the test above. Without it, `getId` could return
    /// `byte[0]` and `isValid` `false` unconditionally and the previous test
    /// would still pass — measuring one branch and calling it coverage.
    ///
    /// The 32-byte id is itself a CratonVM stand-in (this VM does not surface
    /// the real negotiated session id); what is pinned here is only that a
    /// session which DID negotiate is not dragged into the null-session
    /// branch by this change.
    #[test]
    fn a_session_that_negotiated_keeps_its_id_and_its_validity() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();

        // 3-field connect shape (retired by E42; the merged arm still answers
        // it) with a real stream id.
        let live = ctx.alloc_object(cratonvm_types::ClassId::new(0), 3);
        ctx.set_field(live, 2, Value::Int(0)); // id 0 is a VALID stream id
                                               // E42 — the 4-field accept/NEW-13 shape, which is what the tree mints
                                               // TODAY. Two rows, because `>= 0` is the boundary and `0` is on it:
                                               // `SSLServerSocket.accept` writes `RUSTLS_SOCK_ID_BASE + stream_id`, so
                                               // its ids are large, while `invalidate()`/`close()` bugs in this family
                                               // have historically produced exactly `Int(0)`. Both must read as
                                               // negotiated, which is what makes the merged arm a NO-OP for the accept
                                               // shape rather than a change to it.
        let live4 = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(live4, 2, Value::Int(0));
        ctx.set_field(live4, 3, Value::Object(None));
        let live4_accept = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(
            live4_accept,
            2,
            Value::Int(crate::servlet::RUSTLS_SOCK_ID_BASE + 3),
        );
        ctx.set_field(live4_accept, 3, Value::Object(None));
        // 8-field engine shape, slot 2 = the isValid flag, set.
        let engine_ok = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(engine_ok, 2, Value::Int(1));
        // 8-field engine shape BEFORE a handshake — the DOOR 2 state.
        let engine_pre = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(engine_pre, 2, Value::Int(0));

        let get_id = r
            .find("javax/net/ssl/SSLSession", "getId", "()[B")
            .expect("getId registered");
        let is_valid = r
            .find("javax/net/ssl/SSLSession", "isValid", "()Z")
            .expect("isValid registered");

        for (what, sess, want_len, want_valid) in [
            ("3-field, stream id 0", live, 32usize, 1i32),
            ("4-field, stream id 0", live4, 32, 1),
            ("4-field, accept-shaped offset id", live4_accept, 32, 1),
            ("8-field, negotiated", engine_ok, 32, 1),
            ("8-field, pre-handshake", engine_pre, 0, 0),
        ] {
            let this = &[Value::Object(Some(sess))];
            match get_id(&mut ctx, this) {
                Ok(Some(Value::Object(Some(a)))) => {
                    assert_eq!(ctx.array_length(a), want_len, "getId: {what}")
                }
                other => panic!("getId must return an array for {what}, got {other:?}"),
            }
            assert_eq!(
                is_valid(&mut ctx, this).unwrap(),
                Some(Value::Int(want_valid)),
                "isValid: {what}"
            );
        }
    }

    /// `getProtocol`/`getCipherSuite` are contracted non-null by JSSE, and
    /// both used to hand back `ctx.get_field(..)` raw. `http2.rs` mints a
    /// 6-field session and never writes a slot of it, so that shape produced a
    /// null String from a method that cannot return one.
    #[test]
    fn an_unpopulated_session_shape_answers_the_sentinel_not_a_null_string() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
        let this = &[Value::Object(Some(sess))];

        let proto = r
            .find(
                "javax/net/ssl/SSLSession",
                "getProtocol",
                "()Ljava/lang/String;",
            )
            .expect("getProtocol registered");
        match proto(&mut ctx, this) {
            Ok(Some(Value::Object(Some(s)))) => assert_eq!(
                ctx.read_string(s).as_deref(),
                Some(crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL)
            ),
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
            Ok(Some(Value::Object(Some(s)))) => assert_eq!(
                ctx.read_string(s).as_deref(),
                Some(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE)
            ),
            other => panic!("getCipherSuite must never return a null String, got {other:?}"),
        }
    }

    /// §2's argument, mechanised on THIS file's list rather than argued: the
    /// sentinel is safe to return only because it can never be the outcome of
    /// a handshake. `SUPPORTED_CIPHER_SUITE_NAMES` is what this VM claims to
    /// support through `getSupportedCipherSuites`, so if the sentinel ever
    /// appears in it the reason for returning the sentinel is gone.
    #[test]
    fn the_sentinel_is_never_offerable_and_the_fabrication_always_was() {
        assert!(
            !SUPPORTED_CIPHER_SUITE_NAMES
                .contains(&crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE),
            "the null-session sentinel must never be an offerable suite — that \
             unofferability is the whole reason it is not a fabrication"
        );
        assert!(
            SUPPORTED_CIPHER_SUITE_NAMES.contains(&"TLS_AES_256_GCM_SHA384"),
            "the literal this lane REMOVED is offerable, which is exactly why no \
             caller could tell it from a real negotiation"
        );
    }

    /// E31 — THE SLOT COLLISION, as a test rather than as a nomination.
    ///
    /// `putValue` used to write its `java.util.HashMap` into
    /// `num_fields - 1`, which on the then-3-field null-session shape was the
    /// STREAM ID. That is the slot `session_has_negotiated` reads, so a single
    /// `putValue` turned `Int(-1)` into an object reference, took the
    /// predicate's defensive `_ => true` arm, and handed the session back its
    /// 32-byte fabricated id and `isValid() == true`.
    ///
    /// This is the whole defect in one assertion: **do the writes, then re-ask
    /// the two accessors E12/E22 fixed.** Jetty's
    /// `SecureRequestCustomizer.retrieveSni()` is the real caller.
    #[test]
    fn a_put_value_cannot_resurrect_the_null_sessions_id_or_validity() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();
        // The DOOR 1 shape: [0]=proto [1]=cipher [2]=tls_id = -1.
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 3);
        ctx.set_field(sess, 2, Value::Int(-1));

        let name = ctx.create_string("org.eclipse.jetty.sni.host");
        let value = ctx.create_string("example.test");
        let put = r
            .find(
                "javax/net/ssl/SSLSession",
                "putValue",
                "(Ljava/lang/String;Ljava/lang/Object;)V",
            )
            .expect("putValue registered");
        put(
            &mut ctx,
            &[
                Value::Object(Some(sess)),
                Value::Object(Some(name)),
                Value::Object(Some(value)),
            ],
        )
        .expect("putValue must not fail");

        assert_eq!(
            ctx.get_field(sess, 2),
            Value::Int(-1),
            "putValue must not overwrite NEW13_SESS_TLSID on a shape with no \
             attribute slot — that slot is what says 'nothing was negotiated'"
        );

        let this = &[Value::Object(Some(sess))];
        let get_id = r
            .find("javax/net/ssl/SSLSession", "getId", "()[B")
            .expect("getId registered");
        match get_id(&mut ctx, this) {
            Ok(Some(Value::Object(Some(a)))) => assert_eq!(
                ctx.array_length(a),
                0,
                "a putValue must not give an unhandshaked session an id"
            ),
            other => panic!("getId must return an array, got {other:?}"),
        }
        let is_valid = r
            .find("javax/net/ssl/SSLSession", "isValid", "()Z")
            .expect("isValid registered");
        assert_eq!(
            is_valid(&mut ctx, this).unwrap(),
            Some(Value::Int(0)),
            "a putValue must not make an unhandshaked session valid"
        );
    }

    /// E42 — THE SAME QUESTION AT THE WIDTH THAT SHIPS.
    ///
    /// The test above pins the retired 3-field shape, where the fix was to
    /// REFUSE (`sslsess_attrs_slot` -> `None`). `NEW13_SSL_SESS_FIELDS` is 4
    /// now, so the live null session takes the *other* branch: slot 3 is a real
    /// attribute slot, the write happens, and the thing that must not move is
    /// slot 2. That makes this the executable form of the whole E42 bargain —
    /// the API works AND the stream id survives it — and it is a different
    /// assertion from the width-3 one, not a rename of it.
    ///
    /// The three follow-up assertions are the measured consequences that were
    /// reported when the widening landed without `session_has_negotiated`'s arm
    /// merge: a `putValue` flipped `isValid()` back to `true` and `getId()`
    /// back to 32 fabricated bytes through an unrelated API. Note they would
    /// pass here for the WRONG reason if the arm merge were reverted — slot 2
    /// still reads `Int(-1)` — which is why
    /// `the_null_socket_session_has_no_id_and_is_not_valid` runs at width 4
    /// too. That test is the one that fails on a reverted arm; this one is the
    /// one that fails if the attribute slot regresses onto slot 2.
    #[test]
    fn a_widened_null_session_put_value_does_not_touch_the_stream_id() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();
        // The width-4 DOOR 1 shape: [0]=proto [1]=cipher [2]=tls_id = -1,
        // [3]=attrs.
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(sess, 2, Value::Int(-1));
        ctx.set_field(sess, 3, Value::Object(None));

        let name = ctx.create_string("org.eclipse.jetty.sni.host");
        let value = ctx.create_string("example.test");
        let put = r
            .find(
                "javax/net/ssl/SSLSession",
                "putValue",
                "(Ljava/lang/String;Ljava/lang/Object;)V",
            )
            .expect("putValue registered");
        put(
            &mut ctx,
            &[
                Value::Object(Some(sess)),
                Value::Object(Some(name)),
                Value::Object(Some(value)),
            ],
        )
        .expect("putValue must not fail");

        assert!(
            matches!(ctx.get_field(sess, 3), Value::Object(Some(_))),
            "slot 3 IS the attribute slot at NEW13_SSL_SESS_FIELDS = 4 — the \
             widening exists so this write has somewhere to land; if it is \
             None, `sslsess_attrs_slot`'s `4 => Some(3)` row has regressed"
        );
        assert_eq!(
            ctx.get_field(sess, 2),
            Value::Int(-1),
            "and it must NOT have landed on the stream id. `num_fields - 1` is \
             3 here as well, so a reader who 'simplifies' `sslsess_attrs_slot` \
             back to that spelling passes this — the guard is the 6-field row, \
             not this one"
        );

        let this = &[Value::Object(Some(sess))];
        let get_id = r
            .find("javax/net/ssl/SSLSession", "getId", "()[B")
            .expect("getId registered");
        match get_id(&mut ctx, this) {
            Ok(Some(Value::Object(Some(a)))) => assert_eq!(
                ctx.array_length(a),
                0,
                "a putValue must not give an unhandshaked session an id"
            ),
            other => panic!("getId must return an array, got {other:?}"),
        }
        let is_valid = r
            .find("javax/net/ssl/SSLSession", "isValid", "()Z")
            .expect("isValid registered");
        assert_eq!(
            is_valid(&mut ctx, this).unwrap(),
            Some(Value::Int(0)),
            "a putValue must not make an unhandshaked session valid"
        );
    }

    // -----------------------------------------------------------------------
    // F18 — the four doors that had no real-mode registration, the one bit
    // behind `isValid()`, and the width table that stops a session reading
    // another connection's certificate chain
    // -----------------------------------------------------------------------

    /// **A registration census, not a behaviour test.** Each of these four was
    /// absent from real-JDK mode entirely, and the failure mode of an absent
    /// registration on `javax/net/ssl/SSLSession` is not a wrong answer — it is
    /// `AbstractMethodError`, because the interface declaration carries no Code
    /// attribute. A body cannot be tested until it exists, so this asserts that
    /// it exists.
    ///
    /// `invalidate` is F10-1 NOMINATION 3; `getPeerHost`/`getPeerPort` are the
    /// accessor half of E12-1's residual 4 (which recorded the *producer* and
    /// did not notice the accessor was missing); `getSessionContext` had zero
    /// registrations anywhere in the crate, in either mode.
    #[test]
    fn the_four_unregistered_real_mode_session_doors_are_registered() {
        let r = session_registry();
        let cls = "javax/net/ssl/SSLSession";
        for (name, desc) in [
            ("invalidate", "()V"),
            ("getPeerHost", "()Ljava/lang/String;"),
            ("getPeerPort", "()I"),
            ("getSessionContext", "()Ljavax/net/ssl/SSLSessionContext;"),
        ] {
            assert!(
                r.find(cls, name, desc).is_some(),
                "{cls}.{name}{desc} has no real-JDK-mode registration. It is an \
                 abstract interface declaration with no Code attribute, so an \
                 un-intercepted call is an AbstractMethodError in the mode \
                 --jdk-only runs — not a wrong value, a thrown Error. \
                 See docs/known-issues/jdk-only/F18-1-*.md."
            );
        }
    }

    /// **The marker-collision invariant, mechanised in the file that now
    /// depends on it.**
    ///
    /// `net_phase_e::HTTPS_CLIENT_SESSION_MARKER` is written into slot 2 of
    /// every HTTPS client session so `session_has_negotiated` reads it as
    /// "negotiated". `peer_certs_for_session` also reads slot 2 — as a
    /// `servlet` registry KEY. The two uses are only compatible while the
    /// marker cannot name a real socket: if it ever could, one connection
    /// would be handed another connection's peer certificate chain, which is a
    /// far worse failure than the `isValid()` one F10 fixed.
    ///
    /// F10-1 §4.1 argued this from the constants. This asserts it, so that
    /// moving any one of the four numbers fails a build instead of a comment.
    #[test]
    fn the_https_session_marker_can_never_name_a_real_socket() {
        use crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER as M;
        assert!(
            M > 0,
            "the marker must be >= 0 or `session_has_negotiated` reads it as \
             'never negotiated' — the defect F10-1 fixed"
        );
        for (name, base) in [
            (
                "PENDING_CONNECT_SOCK_ID_BASE",
                crate::servlet::PENDING_CONNECT_SOCK_ID_BASE,
            ),
            (
                "PENDING_LAYERED_SOCK_ID_BASE",
                crate::servlet::PENDING_LAYERED_SOCK_ID_BASE,
            ),
            ("RUSTLS_SOCK_ID_BASE", crate::servlet::RUSTLS_SOCK_ID_BASE),
        ] {
            assert!(
                M < base,
                "HTTPS_CLIENT_SESSION_MARKER ({M:#x}) must stay strictly below \
                 servlet::{name} ({base:#x}), or an HTTPS session's slot 2 \
                 names a real socket id and `peer_certs_for_session` hands it \
                 THAT socket's peer certificate chain"
            );
        }
    }

    /// The width table behind `session_stream_id`, which is the fix for the
    /// cross-connection read E31-1 NOMINATION 4 (re-raising E22-1 NOMINATION B)
    /// recorded and nobody had landed.
    ///
    /// The old test was `> NEW13_SESS_TLSID` — "three or more fields" — so on
    /// the 8-field engine session slot 2, the `isValid` FLAG, was looked up in
    /// the socket registry. `servlet::s2_next_free_id` starts its counter at
    /// `1`, so a valid engine session asked the registry for the chain of the
    /// FIRST socket the process ever opened.
    #[test]
    fn slot_two_is_only_a_stream_id_on_the_widths_where_it_is_one() {
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();

        // Width 4 — the NEW-13 shape and `SSLServerSocket.accept`'s shape.
        // Slot 2 IS a stream id here, and this is the ONLY width that ships.
        let four = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(four, 2, Value::Int(7));
        assert_eq!(super::session_stream_id(&ctx, four), Some(7));

        // ... and `-1`, every minter's "never connected" sentinel, is not one.
        ctx.set_field(four, 2, Value::Int(-1));
        assert_eq!(super::session_stream_id(&ctx, four), None);

        // Width 8 — the engine session. Slot 2 is the `isValid` flag. Both of
        // its values are plausible registry keys, and BOTH must be refused.
        for flag in [0, 1] {
            let eight = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
            ctx.set_field(eight, 2, Value::Int(flag));
            assert_eq!(
                super::session_stream_id(&ctx, eight),
                None,
                "slot 2 of the 8-field engine shape is the isValid flag, not a \
                 stream id; reading it as one looks up socket id {flag} and \
                 `s2_next_free_id` hands out id 1 first"
            );
        }

        // Width 6 — `tls.rs`'s shape, where slot 2 is `tls.rs::SES_VALID`.
        let six = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
        ctx.set_field(six, 2, Value::Int(1));
        assert_eq!(super::session_stream_id(&ctx, six), None);
    }

    /// `invalidate()` moves `isValid()` — the whole point of registering it —
    /// and moves NOTHING else. MEASURED, HotSpot 25.0.3+9-LTS, on a session
    /// that genuinely negotiated (`scratchpad/f18/F18SessionContract.java`,
    /// three runs byte-identical): after `invalidate()` the id keeps its 32
    /// bytes *and its exact contents*, and the cipher and protocol survive.
    ///
    /// The id half is the assertion that matters, because it is the one a
    /// plausible "simplification" breaks: gating `getId` on `isValid` instead
    /// of on `session_has_negotiated` would make `invalidate()` erase the id,
    /// trading a new divergence for the fixed one.
    #[test]
    fn invalidate_moves_is_valid_and_nothing_else() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();

        // A width-4 session that DID negotiate: slot 2 is a real stream id.
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(sess, 2, Value::Int(11));
        ctx.set_field(sess, 3, Value::Object(None));
        let this = &[Value::Object(Some(sess))];

        let is_valid = r
            .find("javax/net/ssl/SSLSession", "isValid", "()Z")
            .expect("isValid registered");
        let get_id = r
            .find("javax/net/ssl/SSLSession", "getId", "()[B")
            .expect("getId registered");
        let invalidate = r
            .find("javax/net/ssl/SSLSession", "invalidate", "()V")
            .expect("invalidate registered");

        assert_eq!(
            is_valid(&mut ctx, this).unwrap(),
            Some(Value::Int(1)),
            "a session carrying stream id 11 negotiated, and nothing has \
             invalidated it yet"
        );
        let id_before = match get_id(&mut ctx, this) {
            Ok(Some(Value::Object(Some(a)))) => (0..ctx.array_length(a))
                .map(|i| ctx.get_array_element(a, i))
                .collect::<Vec<_>>(),
            other => panic!("getId must return an array, got {other:?}"),
        };
        assert_eq!(id_before.len(), 32, "a negotiated session has a 32-byte id");

        invalidate(&mut ctx, this).expect("invalidate must not fail");

        assert_eq!(
            is_valid(&mut ctx, this).unwrap(),
            Some(Value::Int(0)),
            "invalidate() must move isValid() to false — at THIS width. Before \
             F18 the only `invalidate` in the crate was `tls.rs`'s, gated \
             `> SES_CREATION_TIME`, so it no-opped here and the session stayed \
             valid after being invalidated"
        );
        let id_after = match get_id(&mut ctx, this) {
            Ok(Some(Value::Object(Some(a)))) => (0..ctx.array_length(a))
                .map(|i| ctx.get_array_element(a, i))
                .collect::<Vec<_>>(),
            other => panic!("getId must return an array, got {other:?}"),
        };
        assert_eq!(
            id_before, id_after,
            "MEASURED on HotSpot: invalidate() leaves the session id byte-for-byte \
             intact. `getId` is gated on `session_has_negotiated` ALONE and must \
             not be 'simplified' onto `session_is_valid`"
        );

        // Idempotent, as measured.
        invalidate(&mut ctx, this).expect("second invalidate must not fail");
        assert_eq!(is_valid(&mut ctx, this).unwrap(), Some(Value::Int(0)));
    }

    /// MUTATION CHECK for the test above, in both directions the invalidated
    /// bit could be wrong.
    ///
    /// Without this, `session_is_valid` could answer `false` for everything —
    /// or `session_mark_invalidated` could mark every session in the process —
    /// and `invalidate_moves_is_valid_and_nothing_else` would still pass,
    /// because it only ever looks at one object.
    #[test]
    fn invalidating_one_session_does_not_invalidate_another() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();

        let a = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(a, 2, Value::Int(21));
        let b = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(b, 2, Value::Int(22));

        let is_valid = r
            .find("javax/net/ssl/SSLSession", "isValid", "()Z")
            .expect("isValid registered");
        let invalidate = r
            .find("javax/net/ssl/SSLSession", "invalidate", "()V")
            .expect("invalidate registered");

        invalidate(&mut ctx, &[Value::Object(Some(a))]).expect("invalidate");
        assert_eq!(
            is_valid(&mut ctx, &[Value::Object(Some(a))]).unwrap(),
            Some(Value::Int(0))
        );
        assert_eq!(
            is_valid(&mut ctx, &[Value::Object(Some(b))]).unwrap(),
            Some(Value::Int(1)),
            "the invalidated bit is per SESSION. HotSpot: invalidating one \
             connection's session leaves a second, untouched connection's \
             session valid (MEASURED, F18SessionContract ARM D)"
        );
    }

    /// The null session is where `invalidate()` must do NOTHING, and this
    /// pins that the newly-registered door did not become a way to *change*
    /// it. MEASURED on HotSpot: on an unconnected `SSLSocket`'s session and a
    /// pre-handshake `SSLEngine`'s alike, every accessor reads identically
    /// before and after `invalidate()`.
    ///
    /// This is also the cheapest regression test for the whole F18 change
    /// against the failure mode F10-1 §7 names: if any edit here loosened
    /// `session_has_negotiated`, the null session would report `isValid()` and
    /// a 32-byte id again — the fabrication E12/E22/E31/E42 removed.
    #[test]
    fn invalidate_changes_nothing_on_a_session_that_negotiated_nothing() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();

        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(sess, 2, Value::Int(-1));
        ctx.set_field(sess, 3, Value::Object(None));
        let this = &[Value::Object(Some(sess))];

        let is_valid = r
            .find("javax/net/ssl/SSLSession", "isValid", "()Z")
            .expect("isValid registered");
        let get_id = r
            .find("javax/net/ssl/SSLSession", "getId", "()[B")
            .expect("getId registered");
        let invalidate = r
            .find("javax/net/ssl/SSLSession", "invalidate", "()V")
            .expect("invalidate registered");

        assert_eq!(is_valid(&mut ctx, this).unwrap(), Some(Value::Int(0)));
        let len_before = match get_id(&mut ctx, this) {
            Ok(Some(Value::Object(Some(arr)))) => ctx.array_length(arr),
            other => panic!("getId must return an array, got {other:?}"),
        };
        assert_eq!(len_before, 0);

        invalidate(&mut ctx, this).expect("invalidate must not fail");

        assert_eq!(
            is_valid(&mut ctx, this).unwrap(),
            Some(Value::Int(0)),
            "still false — there was nothing to invalidate"
        );
        let len_after = match get_id(&mut ctx, this) {
            Ok(Some(Value::Object(Some(arr)))) => ctx.array_length(arr),
            other => panic!("getId must return an array, got {other:?}"),
        };
        assert_eq!(
            len_after, 0,
            "and still byte[0]. An invalidate() that gave the null session an \
             id would be the E42-era slot-2 corruption with a new writer"
        );
    }

    /// `getSessionContext()` answers `null` for a session that is in no
    /// context, and a real carrier for one that is.
    ///
    /// **G7 rewrote this test with the registration it pins.** It used to
    /// assert `null` for BOTH a negotiated and a never-negotiated shape, and
    /// said so deliberately: the interface's own contract permits `null`
    /// ("This context may be unavailable in some environments…" —
    /// `javax/net/ssl/SSLSession.getSessionContext`) and this VM was described
    /// as having no session cache. The second half of that was false —
    /// `net_phase_e` allocates a `javax/net/ssl/SSLSessionContext` carrier for
    /// `SSLContext.get{Client,Server}SessionContext()` and registers its whole
    /// six-method surface — so the door was under-reporting against its own
    /// siblings, not against a capability the VM lacks.
    ///
    /// The tempting wrong fix is still a fabricated context, and the tempting
    /// wrong REFUSAL is still a blanket `null`; the arms below pin the boundary
    /// between them at the measured place: negotiated AND not invalidated.
    /// MEASURED, HotSpot 25.0.3+9-LTS, `scratchpad/g7/TlsProbe.java`.
    #[test]
    fn get_session_context_answers_null_rather_than_fabricating_one() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();
        let get_ctx = r
            .find(
                "javax/net/ssl/SSLSession",
                "getSessionContext",
                "()Ljavax/net/ssl/SSLSessionContext;",
            )
            .expect("getSessionContext registered");
        let invalidate = r
            .find("javax/net/ssl/SSLSession", "invalidate", "()V")
            .expect("invalidate registered");

        // ARM 1 — width-4 socket shape, slot 2 = -1: nothing was negotiated.
        let never = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(never, 2, Value::Int(-1));
        assert_eq!(
            get_ctx(&mut ctx, &[Value::Object(Some(never))]).unwrap(),
            Some(Value::Object(None)),
            "a session that negotiated nothing is in no context — HotSpot \
             answers null on an unconnected socket's session and on a \
             pre-handshake engine's alike"
        );

        // ARM 2 — width-4 socket shape with a real stream id: negotiated and
        // not invalidated, so it IS in a context. This is the row the old
        // unconditional `null` got wrong, and the row netty's
        // `SSLEngineTest.testSessionAfterHandshake0` asserts non-null on.
        let live = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(live, 2, Value::Int(5));
        match get_ctx(&mut ctx, &[Value::Object(Some(live))]).unwrap() {
            Some(Value::Object(Some(_))) => {}
            other => panic!(
                "a negotiated, valid session must answer a real \
                 javax/net/ssl/SSLSessionContext carrier — the same zero-field \
                 object net_phase_e hands out from \
                 SSLContext.getClientSessionContext(), whose six methods it \
                 registers. Got {other:?}"
            ),
        }

        // ARM 3 — and `invalidate()` takes it back out. MEASURED: the second
        // thing invalidate() moves, after isValid().
        invalidate(&mut ctx, &[Value::Object(Some(live))]).expect("invalidate must not fail");
        assert_eq!(
            get_ctx(&mut ctx, &[Value::Object(Some(live))]).unwrap(),
            Some(Value::Object(None)),
            "invalidate() evicts the session from its context; HotSpot's \
             ctx.getSession(id) answers null afterwards where it answered \
             SAME-OBJECT before"
        );
    }

    /// The engine shape's three states, which the socket shape cannot express:
    /// a fresh engine, a handshake in flight, and a completed one. This is the
    /// gate G7 moved off `getId()` and onto this door, so it is tested here and
    /// its absence is tested next door.
    ///
    /// MEASURED, HotSpot 25.0.3+9-LTS (`scratchpad/g7/TlsProbe.java`), sampling
    /// `SSLEngine.getHandshakeSession()` from inside
    /// `X509ExtendedTrustManager.checkServerTrusted(chain, authType, SSLEngine)`:
    /// mid-handshake the session already answers a 32-byte id, a real cipher
    /// suite and `isValid() == true`, and `getSessionContext()` is still `null`.
    /// A gate on validity alone cannot produce that row.
    #[test]
    fn a_handshake_still_in_flight_has_no_session_context_yet() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();
        let get_ctx = r
            .find(
                "javax/net/ssl/SSLSession",
                "getSessionContext",
                "()Ljavax/net/ssl/SSLSessionContext;",
            )
            .expect("getSessionContext registered");

        // Fresh engine: `build_synthetic_ssl_session` writes slot 2 = 0 because
        // `conn.negotiated_cipher_suite()` is None.
        let fresh = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(fresh, 2, Value::Int(0));
        assert_eq!(
            get_ctx(&mut ctx, &[Value::Object(Some(fresh))]).unwrap(),
            Some(Value::Object(None))
        );

        // Mid-handshake: a suite HAS been negotiated (slot 2 = 1) but
        // `engine_session_for` has not recorded this object as belonging to a
        // completed epoch.
        let mid = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(mid, 2, Value::Int(1));
        assert_eq!(
            get_ctx(&mut ctx, &[Value::Object(Some(mid))]).unwrap(),
            Some(Value::Object(None)),
            "mid-handshake the session is valid and has an id, and HotSpot \
             still answers null here"
        );

        // Completed: the key `engine_session_for` inserts is present.
        let done = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(done, 2, Value::Int(1));
        let key = super::gc_stable_objref_key(&ctx, done);
        super::negotiated_session_keys().lock().insert(key);
        let answer = get_ctx(&mut ctx, &[Value::Object(Some(done))]).unwrap();
        // Removed before asserting: `negotiated_session_keys` is a process
        // global and `MockNativeContext` restarts its pointer sequence per
        // instance, so a leaked key can be re-derived by another test's object.
        super::negotiated_session_keys().lock().remove(&key);
        match answer {
            Some(Value::Object(Some(_))) => {}
            other => panic!("a completed engine session must answer a real context. Got {other:?}"),
        }
    }

    /// The other half of the same move: `getId()` must NOT consult
    /// `negotiated_session_keys` any more.
    ///
    /// MEASURED: mid-handshake HotSpot answers `byte[32]`, byte-identical to
    /// the id the completed session then reports. The old second gate answered
    /// `byte[0]` in exactly that state, and Tomcat's `JSSESupport.getSessionId`
    /// tests `length == 0` exactly, so `byte[0]` there presents a live
    /// handshake as an untrackable session.
    ///
    /// The fresh-engine row is the one netty's `SSLEngineTest.testSSLSessionId`
    /// asserts (`assertEquals(0, engine.getSession().getId().length)`), and it
    /// is answered by `session_has_negotiated` alone.
    #[test]
    fn get_id_answers_thirty_two_bytes_while_the_handshake_is_still_running() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();
        let get_id = r
            .find("javax/net/ssl/SSLSession", "getId", "()[B")
            .expect("getId registered");

        // Fresh engine — no suite negotiated. Still byte[0].
        let fresh = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(fresh, 2, Value::Int(0));
        let len = match get_id(&mut ctx, &[Value::Object(Some(fresh))]) {
            Ok(Some(Value::Object(Some(arr)))) => ctx.array_length(arr),
            other => panic!("getId must return an array, got {other:?}"),
        };
        assert_eq!(
            len, 0,
            "a freshly created engine's session has no id — netty's \
             SSLEngineTest.testSSLSessionId asserts exactly this"
        );

        // Mid-handshake — a suite has been negotiated, the epoch key is absent.
        let mid = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(mid, 2, Value::Int(1));
        assert!(
            !super::negotiated_session_keys()
                .lock()
                .contains(&super::gc_stable_objref_key(&ctx, mid)),
            "this arm is only meaningful while the epoch key is absent"
        );
        let len = match get_id(&mut ctx, &[Value::Object(Some(mid))]) {
            Ok(Some(Value::Object(Some(arr)))) => ctx.array_length(arr),
            other => panic!("getId must return an array, got {other:?}"),
        };
        assert_eq!(
            len, 32,
            "mid-handshake HotSpot answers a 32-byte id. If this reads 0, the \
             `engine_shape && !session_is_negotiated(..)` gate has come back to \
             getId — it belongs on getSessionContext, which is the door whose \
             oracle answer actually changes across that boundary. See \
             docs/known-issues/jdk-only/G7-1-*.md §3."
        );
    }

    /// `getPeerHost`/`getPeerPort` must not read slot 3 and 4 on a shape where
    /// they are not the host and port.
    ///
    /// This is the E31-1 §2 defect, which was live in `tls.rs`'s copies: on the
    /// width-4 shape slot 3 is the ATTRIBUTE MAP, so a width-blind
    /// `getPeerHost` returns a `java.util.HashMap` through a
    /// `()Ljava/lang/String;` descriptor as soon as anything has called
    /// `putValue` — and Jetty's `SecureRequestCustomizer.retrieveSni()` does,
    /// on every SSL request. The new registrations use `session_cipher_slot`'s
    /// `>= 6` boundary so this file has one width line and not two.
    ///
    /// HotSpot's measured answers for a session that negotiated nothing are
    /// `null` and `-1`, which is what the width-4 shape with no registry entry
    /// falls through to here.
    #[test]
    fn peer_host_and_port_do_not_read_the_attribute_slot() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();

        // Width 4 with a populated attribute slot — the Jetty state.
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(sess, 2, Value::Int(-1));
        let map = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        ctx.set_field(sess, 3, Value::Object(Some(map)));
        let this = &[Value::Object(Some(sess))];

        let host = r
            .find(
                "javax/net/ssl/SSLSession",
                "getPeerHost",
                "()Ljava/lang/String;",
            )
            .expect("getPeerHost registered");
        let port = r
            .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
            .expect("getPeerPort registered");

        assert_eq!(
            host(&mut ctx, this).unwrap(),
            Some(Value::Object(None)),
            "slot 3 is the ATTRIBUTE MAP at width 4, not the peer host. \
             Returning it would hand a java.util.HashMap back through a \
             ()Ljava/lang/String; descriptor"
        );
        assert_eq!(
            port(&mut ctx, this).unwrap(),
            Some(Value::Int(-1)),
            "HotSpot's measured answer for a session that negotiated nothing"
        );
    }

    /// MUTATION CHECK for the test above: without it, both accessors could
    /// return the sentinel for EVERY shape and still pass. The 6-/8-field
    /// shapes DO carry a peer host at slot 3 and a port at slot 4, and must
    /// report them.
    #[test]
    fn peer_host_and_port_are_read_on_the_shapes_that_carry_them() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();

        let host = r
            .find(
                "javax/net/ssl/SSLSession",
                "getPeerHost",
                "()Ljava/lang/String;",
            )
            .expect("getPeerHost registered");
        let port = r
            .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
            .expect("getPeerPort registered");

        for width in [6, 8] {
            let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), width);
            let h = ctx.create_string("example.test");
            ctx.set_field(sess, 3, Value::Object(Some(h)));
            ctx.set_field(sess, 4, Value::Int(8443));
            let this = &[Value::Object(Some(sess))];
            assert_eq!(
                host(&mut ctx, this).unwrap(),
                Some(Value::Object(Some(h))),
                "slot 3 IS the peer host at width {width}"
            );
            assert_eq!(port(&mut ctx, this).unwrap(), Some(Value::Int(8443)));
        }
    }

    /// `tls.rs::init_ssl_session_fields` seeds the peer-host slot with
    /// `Int(0)`, not a String reference. A `getPeerHost` that returned the raw
    /// slot would hand an `Int` back through a `()Ljava/lang/String;`
    /// descriptor — the same descriptor violation as the attribute-map case,
    /// from the opposite direction.
    ///
    /// The port half is the same defect with a quieter symptom: `http2.rs`'s
    /// 6-field session writes NO slot, so slot 4 is the allocator's zero fill,
    /// and `0` returned through `()I` is a *plausible* value HotSpot never
    /// produces — the shape this directory keeps recording as worse than a
    /// loud one.
    #[test]
    fn an_unwritten_peer_host_slot_is_null_and_an_unwritten_port_is_minus_one() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
        ctx.set_field(sess, 3, Value::Int(0));
        ctx.set_field(sess, 4, Value::Int(0));
        let this = &[Value::Object(Some(sess))];
        let host = r
            .find(
                "javax/net/ssl/SSLSession",
                "getPeerHost",
                "()Ljava/lang/String;",
            )
            .expect("getPeerHost registered");
        let port = r
            .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
            .expect("getPeerPort registered");
        assert_eq!(host(&mut ctx, this).unwrap(), Some(Value::Object(None)));
        assert_eq!(
            port(&mut ctx, this).unwrap(),
            Some(Value::Int(-1)),
            "HotSpot's measured answer when there is no peer. A connected peer \
             never reports port 0, so mapping the zero fill to -1 cannot mask \
             a real port"
        );
    }

    /// G51 — the peer endpoint on the shape that has no slot for one.
    ///
    /// MEASURED, `RSslLiveSession` on `9ae371468` (`target-rel3`), 2026-08-17:
    ///
    /// ```text
    /// CK RSslLiveSession client.peerHost                    = null   WANT localhost
    /// CK RSslLiveSession client.peerPort.isServerPort       = false  WANT true
    /// CK RSslLiveSession attrs.shadow.peerHost              = null   WANT localhost
    /// CK RSslLiveSession attrs.shadow.peerPort.isServerPort = false  WANT true
    /// CK RSslLiveSession server.peerPort.isPositive         = false  WANT true
    /// ```
    ///
    /// The trap is armed in the same breath: slot 3 carries a populated
    /// ATTRIBUTE MAP, which is the state E31-1 §2 records — a width-blind read
    /// hands a `java.util.HashMap` back through a `()Ljava/lang/String;`
    /// descriptor. The recorded endpoint must be answered from the SIDE TABLE
    /// and the attribute slot must stay untouched, which is the whole reason
    /// G44-1 §4 rejected widening the session shape instead.
    #[test]
    fn a_recorded_peer_endpoint_answers_the_shape_that_has_no_slot_for_one() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();

        // The width-4 HTTPS/accept shape, with the attribute map populated.
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(
            sess,
            2,
            Value::Int(crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER),
        );
        let map = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        ctx.set_field(sess, 3, Value::Object(Some(map)));
        let this = &[Value::Object(Some(sess))];

        let host = r
            .find(
                "javax/net/ssl/SSLSession",
                "getPeerHost",
                "()Ljava/lang/String;",
            )
            .expect("getPeerHost registered");
        let port = r
            .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
            .expect("getPeerPort registered");

        // MUTATION GUARD, and the before-state: with nothing recorded the two
        // accessors must still answer HotSpot's never-negotiated pair, because
        // that is what `RSslNullSession` (89 checks, green) asserts.
        assert_eq!(host(&mut ctx, this).unwrap(), Some(Value::Object(None)));
        assert_eq!(port(&mut ctx, this).unwrap(), Some(Value::Int(-1)));

        super::record_session_peer_endpoint(&ctx, sess, "localhost", 45123);
        match host(&mut ctx, this).unwrap() {
            Some(Value::Object(Some(s))) => {
                assert_eq!(ctx.read_string(s).as_deref(), Some("localhost"))
            }
            other => panic!("getPeerHost must answer the recorded host, got {other:?}"),
        }
        assert_eq!(port(&mut ctx, this).unwrap(), Some(Value::Int(45123)));
        assert_eq!(
            ctx.get_field(sess, 3),
            Value::Object(Some(map)),
            "the attribute slot is not the peer host and must not be disturbed \
             — E31-1 §2, and the reason G44-1 §4 refuses to widen this shape"
        );
    }

    /// The recorded endpoint must NOT outrank the slots on the shapes that
    /// genuinely carry a peer host and port.
    ///
    /// The width branch runs first, deliberately: the 6- and 8-field shapes are
    /// written by their own minters and a side-table row for one of them would
    /// be a second source of truth for a fact the object already states. This
    /// is the mutation check on the ORDER of the two lookups — a reader that
    /// consulted the table first would pass every other test in this file.
    #[test]
    fn a_recorded_endpoint_does_not_shadow_the_slots_that_carry_one() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();
        let host = r
            .find(
                "javax/net/ssl/SSLSession",
                "getPeerHost",
                "()Ljava/lang/String;",
            )
            .expect("getPeerHost registered");
        let port = r
            .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
            .expect("getPeerPort registered");

        for width in [6, 8] {
            let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), width);
            let h = ctx.create_string("example.test");
            ctx.set_field(sess, 3, Value::Object(Some(h)));
            ctx.set_field(sess, 4, Value::Int(8443));
            super::record_session_peer_endpoint(&ctx, sess, "wrong.test", 1);
            let this = &[Value::Object(Some(sess))];
            assert_eq!(
                host(&mut ctx, this).unwrap(),
                Some(Value::Object(Some(h))),
                "slot 3 wins at width {width}"
            );
            assert_eq!(port(&mut ctx, this).unwrap(), Some(Value::Int(8443)));
        }
    }

    /// "Nothing to say" must stay ABSENT, not become a recorded blank.
    ///
    /// An absent row means "nobody recorded an endpoint", and both readers fall
    /// THROUGH it to `session_stream_id` and the socket registry — which is the
    /// only answer the `SSLSocketFactory.createSocket` client shape has. A row
    /// of `("", -1)` would shadow that. HotSpot's never-connected pair
    /// (`null`, `-1`) is measured in `session_peer_endpoint_table`'s table.
    #[test]
    fn an_empty_endpoint_is_not_recorded_at_all() {
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        super::record_session_peer_endpoint(&ctx, sess, "", -1);
        assert!(
            super::session_peer_endpoint(&ctx, sess).is_none(),
            "an empty host and a non-positive port say nothing, and a row that \
             says nothing shadows the socket-registry fallback"
        );
        super::record_session_peer_endpoint(&ctx, sess, "", 4711);
        assert_eq!(
            super::session_peer_endpoint(&ctx, sess),
            Some((String::new(), 4711)),
            "a port with no host is still an answer for getPeerPort"
        );
    }

    /// G51 — the SERVER session's own certificate chain.
    ///
    /// MEASURED, `RSslLiveSession` on `9ae371468`: `server.localPrincipal =
    /// null WANT CN=localhost`, `server.localPrincipal.class = null`, and
    /// `server.localCertificates.length = -1 WANT 1`. All three read
    /// `session_local_certs_table`, whose only writer before G51 was an
    /// open-coded insert at the tail of `build_synthetic_ssl_session` — a
    /// function `SSLServerSocket.accept()` never reaches.
    ///
    /// The empty-chain contract is the other half and is NOT a tidiness rule:
    /// `client.localCertificates = null` and `client.localPrincipal = null` are
    /// measured GREEN rows for a client with no configured identity, and
    /// `ssl_security`'s two readers answer `null` on an empty chain. A writer
    /// that recorded an empty vector would turn those into `Certificate[0]`.
    #[test]
    fn the_local_chain_writer_records_a_chain_and_declines_an_empty_one() {
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        super::record_local_cert_chain(&ctx, sess, vec![]);
        assert!(
            super::local_certs_for_session(&ctx, sess).is_empty(),
            "an empty chain must leave the table empty: getLocalCertificates() \
             answers null there, which is the measured client-side row"
        );
        super::record_local_cert_chain(&ctx, sess, vec![vec![0x30, 0x82, 0x03]]);
        assert_eq!(
            super::local_certs_for_session(&ctx, sess),
            vec![vec![0x30u8, 0x82, 0x03]],
            "the accepted server session's local chain is what \
             getLocalCertificates() returns and what getLocalPrincipal() takes \
             its subject from — one fact, three rows"
        );
    }

    /// A dual-stack listener reports a loopback client as `::ffff:127.0.0.1`.
    ///
    /// MEASURED on HotSpot (`scratchpad/g51/G51Probe.java`): the server-side
    /// `getPeerHost()` is `127.0.0.1`, equal to the accepted socket's own
    /// `getInetAddress().getHostAddress()`, and it is never reverse-resolved to
    /// a name. A real IPv6 peer keeps its own spelling.
    #[test]
    fn an_ipv4_mapped_peer_address_is_reported_in_ipv4_spelling() {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
        let mapped = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped(), 53114));
        assert_eq!(
            super::socket_addr_endpoint(mapped),
            ("127.0.0.1".to_string(), 53114)
        );
        let v4 = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 53114));
        assert_eq!(
            super::socket_addr_endpoint(v4),
            ("127.0.0.1".to_string(), 53114)
        );
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 53114));
        assert_eq!(
            super::socket_addr_endpoint(v6),
            ("::1".to_string(), 53114),
            "a genuine IPv6 peer is not an IPv4-mapped one and keeps its \
             spelling"
        );
    }

    /// The two peer-identity doors must see ONE chain.
    ///
    /// `t27_tls::getPeerCertificates` and `ssl_security::getPeerPrincipal` are
    /// registered by different registrars and used to read different sources —
    /// the object-keyed table and the socket registry. HotSpot's contract makes
    /// disagreement impossible by construction: MEASURED, the principal IS the
    /// leaf certificate's subject
    /// (`getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal())`).
    ///
    /// This pins the half a unit test can reach without a TLS peer: that the
    /// chain an HTTPS client session was given via `record_client_peer_chain`
    /// is visible through `peer_certs_for_session`, which is now the only
    /// function either door consults. The width-4 HTTPS session carries
    /// `HTTPS_CLIENT_SESSION_MARKER` in slot 2 and has NO socket-registry
    /// entry, so before F18 the principal door found nothing here.
    #[test]
    fn the_object_keyed_chain_is_visible_to_the_one_resolver() {
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(
            sess,
            2,
            Value::Int(crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER),
        );
        assert!(
            super::peer_certs_for_session(&ctx, sess).is_empty(),
            "nothing recorded yet"
        );
        super::record_client_peer_chain(&ctx, sess, vec![vec![0x30, 0x82, 0x01]]);
        assert_eq!(
            super::peer_certs_for_session(&ctx, sess),
            vec![vec![0x30u8, 0x82, 0x01]],
            "the HTTPS client session's chain lives in the OBJECT-keyed table; \
             its slot 2 marker deliberately misses the socket registry, so a \
             resolver that consulted only the registry — which is what \
             getPeerPrincipal did — throws SSLPeerUnverifiedException on a \
             completed handshake. F10-1 NOMINATION 2"
        );
    }

    /// MUTATION CHECK for the test above: without it, `peer_certs_for_session`
    /// could return a non-empty chain for anything and still pass. A session
    /// nobody recorded a chain for must resolve to empty, because that is what
    /// makes `getPeerCertificates`/`getPeerPrincipal` throw
    /// `SSLPeerUnverifiedException` — the refusal `RSslNullSession` asserts.
    #[test]
    fn a_session_with_no_recorded_chain_resolves_to_empty() {
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(sess, 2, Value::Int(-1));
        assert!(super::peer_certs_for_session(&ctx, sess).is_empty());
    }

    /// MUTATION CHECK for `a_widened_null_session_put_value_does_not_touch_the_stream_id`.
    /// Without it, `putValue` could no-op
    /// for EVERY shape and the collision test would still pass — the classic
    /// "measured the refusal, called it coverage" shape. The 8-field engine
    /// session has a dedicated attribute slot and must still round-trip.
    #[test]
    fn a_shape_with_a_real_attribute_slot_still_round_trips() {
        use crate::test_utils::MockNativeContext;
        let r = session_registry();
        let mut ctx = MockNativeContext::new();
        // 8-field engine shape: slot 2 = isValid flag, slot 7 = attrs.
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(sess, 2, Value::Int(1));

        let name = ctx.create_string("k");
        let value = ctx.create_string("v");
        let put = r
            .find(
                "javax/net/ssl/SSLSession",
                "putValue",
                "(Ljava/lang/String;Ljava/lang/Object;)V",
            )
            .expect("putValue registered");
        put(
            &mut ctx,
            &[
                Value::Object(Some(sess)),
                Value::Object(Some(name)),
                Value::Object(Some(value)),
            ],
        )
        .expect("putValue must not fail");
        assert!(
            matches!(ctx.get_field(sess, 7), Value::Object(Some(_))),
            "slot 7 IS the attribute slot on the 8-field shape — putValue must \
             still store there, or `sslsess_attrs_slot` has disabled the API"
        );
        assert_eq!(
            ctx.get_field(sess, 2),
            Value::Int(1),
            "and it must not have touched the isValid flag"
        );
    }

    /// The width->slot rules, pinned as a table so a future shape has to
    /// declare which convention it uses instead of inheriting one silently.
    /// The 6-field row is the one that was wrong (`>= 7`): `tls.rs`'s and
    /// `http2.rs`'s sessions are cipher-first like the 8-field engine shape,
    /// not protocol-first like the narrow ones.
    #[test]
    fn the_session_slot_conventions_are_split_at_six_fields_not_seven() {
        for n in [0usize, 1] {
            assert_eq!(session_cipher_slot(n), None, "width {n} carries neither");
            assert_eq!(session_proto_slot(n), None, "width {n} carries neither");
        }
        for n in [2usize, 3, 4] {
            assert_eq!(
                session_proto_slot(n),
                Some(0),
                "width {n} is protocol-first"
            );
            assert_eq!(
                session_cipher_slot(n),
                Some(1),
                "width {n} is protocol-first"
            );
        }
        for n in [6usize, 8] {
            assert_eq!(session_cipher_slot(n), Some(0), "width {n} is cipher-first");
            assert_eq!(session_proto_slot(n), Some(1), "width {n} is cipher-first");
        }
    }

    // =====================================================================
    // G25 - the int that was written into a reference slot.
    // Record: docs/known-issues/jdk-only/
    //         G25-1-the-int-written-into-a-reference-slot-20260817.md
    // =====================================================================

    fn g25_state(bind_address: &str, bind_display: &str) -> SslServerSocketState {
        SslServerSocketState {
            listener_id: 7,
            local_port: 60553,
            closed: 0,
            bound: 1,
            bind_address: bind_address.to_string(),
            bind_display: bind_display.to_string(),
        }
    }

    /// The whole point of the change: nothing in `SSLServerSocket`'s lifecycle
    /// is stored in an object slot any more, because none of the six slots
    /// `java.net.ServerSocket` declares means what this file wanted to put
    /// there. Slot 0 is `impl` - a REFERENCE - and an `Int` written there was
    /// never readable, which is why `getImpl()` was null and every inherited
    /// method NPE'd.
    ///
    /// The test reads this file's own source, because the defect is a WRITE
    /// that compiles, registers and silently does nothing: there is no runtime
    /// observation of it from inside this crate, only the absence of the call.
    #[test]
    fn the_ssl_server_socket_lifecycle_is_never_written_into_an_object_slot() {
        let source = include_str!("t27_tls.rs");
        // Assembled from fragments so this test's own source does not contain
        // the needles it searches for - the same trick, and the same reason, as
        // `the_only_rustls_suite_spelling_left_is_the_adapters_own`.
        let write = format!("{}{}", "ctx.set_", "field(");
        for (receiver, slot) in [
            ("obj", "SSS_LISTENER_ID"),
            ("obj", "SSS_LOCAL_PORT"),
            ("obj", "SSS_CLOSED"),
            ("this", "SSS_LISTENER_ID"),
            ("this", "SSS_CLOSED"),
        ] {
            let needle = format!("{write}{receiver}, {slot}");
            assert!(
                !source.contains(&needle),
                "`{needle}` is back. Slots 0/1/2/3 of a real \
                 javax.net.ssl.SSLServerSocket are impl (a REFERENCE) / created / \
                 bound / closed; `ssl_server_socket_states` is the authority."
            );
        }
    }

    /// `isBound` / `getInetAddress` / `getLocalSocketAddress` / `toString` are
    /// ONE contract. `java.net.ServerSocket.toString()` short-circuits to the
    /// constant `"ServerSocket[unbound]"` while `isBound()` is false and
    /// otherwise dereferences `impl`, so registering `isBound` without
    /// `toString` walks the inherited body into a null `impl` and converts a
    /// row that AGREED with HotSpot into an NPE. The mirror of
    /// `net_phase_e`'s
    /// `ssl_server_socket_bound_identity_rows_are_not_registered_piecemeal`,
    /// from the side that now owns them.
    #[test]
    fn sss_bind_identity_rows_move_together() {
        let mut r = NativeMethodRegistry::new();
        super::register_sslserversocket(&mut r);
        let rows = [
            ("isBound", "()Z"),
            ("getInetAddress", "()Ljava/net/InetAddress;"),
            ("getLocalSocketAddress", "()Ljava/net/SocketAddress;"),
            ("toString", "()Ljava/lang/String;"),
        ];
        let mut present = 0usize;
        for (method, descriptor) in rows {
            if r.find("javax/net/ssl/SSLServerSocket", method, descriptor)
                .is_some()
            {
                present += 1;
            }
        }
        assert_eq!(
            present,
            rows.len(),
            "{present} of {} bind-identity rows registered; they are one contract",
            rows.len()
        );
    }

    /// The three rows measured as `AbstractMethodError` (`G16Sweep`), and the
    /// setters that make their getters read-backs rather than constants.
    #[test]
    fn sss_abstract_method_error_rows_are_registered_with_their_setters() {
        let mut r = NativeMethodRegistry::new();
        super::register_sslserversocket(&mut r);
        for (method, descriptor) in [
            ("getEnabledCipherSuites", "()[Ljava/lang/String;"),
            ("getSupportedCipherSuites", "()[Ljava/lang/String;"),
            ("setEnabledCipherSuites", "([Ljava/lang/String;)V"),
            ("getUseClientMode", "()Z"),
            ("setUseClientMode", "(Z)V"),
            ("getEnableSessionCreation", "()Z"),
            ("setEnableSessionCreation", "(Z)V"),
        ] {
            assert!(
                r.find("javax/net/ssl/SSLServerSocket", method, descriptor)
                    .is_some(),
                "SSLServerSocket.{method}{descriptor} is abstract with no Code \
                 attribute; without a native it raises AbstractMethodError"
            );
        }
    }

    /// The other half of `net_phase_e`'s
    /// `ssl_server_socket_option_registrar_leaves_the_t27_owned_names_alone`,
    /// asserted from this side. `register_t27_natives` runs AFTER
    /// `register_phase_e_networking` (lib.rs 18731 vs 18688) and `register()`
    /// is last-write-wins with no unregister API, so a name added here that
    /// RE.6b already owns does not conflict - it silently deletes RE.6b's
    /// body, and with it the delegate that makes `setSoTimeout` work at all.
    /// `accept()` now ASKS one of those nine through `invoke_virtual`, so
    /// shadowing `getSoTimeout` here would also make the accept timeout read
    /// its own answer.
    #[test]
    fn t27_does_not_shadow_the_re6b_option_surface() {
        let mut r = NativeMethodRegistry::new();
        super::register_sslserversocket(&mut r);
        for (method, descriptor) in [
            ("getSoTimeout", "()I"),
            ("setSoTimeout", "(I)V"),
            ("getReuseAddress", "()Z"),
            ("setReuseAddress", "(Z)V"),
            ("getReceiveBufferSize", "()I"),
            ("setReceiveBufferSize", "(I)V"),
            ("supportedOptions", "()Ljava/util/Set;"),
            ("getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
            (
                "setOption",
                "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/ServerSocket;",
            ),
        ] {
            assert!(
                r.find("javax/net/ssl/SSLServerSocket", method, descriptor)
                    .is_none(),
                "SSLServerSocket.{method}{descriptor} belongs to net_phase_e's \
                 RE.6b; registering it here runs later and kills that body"
            );
        }
    }

    /// Both renderings, transcribed from HotSpot 25.0.3+9 (`G25Probe`).
    #[test]
    fn sss_to_string_matches_the_oracle() {
        assert_eq!(
            sss_to_string(Some(&g25_state("127.0.0.1", "/127.0.0.1"))),
            "[SSL: ServerSocket[addr=/127.0.0.1,localport=60553]]"
        );
        assert_eq!(
            sss_to_string(Some(&g25_state(SSS_WILDCARD_BIND, SSS_WILDCARD_DISPLAY))),
            "[SSL: ServerSocket[addr=0.0.0.0/0.0.0.0,localport=60553]]"
        );
        assert_eq!(sss_to_string(None), "ServerSocket[unbound]");
    }

    /// MEASURED: `close()` moves `isClosed()` and NOTHING else. Every bind row
    /// answers after close exactly what it answered before.
    #[test]
    fn closing_a_server_socket_does_not_unbind_it() {
        let open = g25_state("127.0.0.1", "/127.0.0.1");
        let closed = sss_closed_state(open.clone());
        assert_eq!(closed.closed, 1);
        assert_eq!(closed.listener_id, -1, "the listener is gone");
        assert_eq!(closed.local_port, open.local_port);
        assert_eq!(closed.bind_address, open.bind_address);
        assert_eq!(closed.bind_display, open.bind_display);
        assert_eq!(sss_to_string(Some(&closed)), sss_to_string(Some(&open)));
    }

    /// A socket with no record is not a socket that is merely unconfigured:
    /// it is one this file cannot accept on, and the miss answers say so.
    #[test]
    fn an_unknown_server_socket_reads_closed_and_unbound() {
        let miss = ssl_server_socket_state_miss();
        assert_eq!(miss.closed, 1);
        assert_eq!(miss.listener_id, -1);
        assert_eq!(miss.local_port, 0);
        assert_eq!(sss_to_string(None), "ServerSocket[unbound]");
    }

    /// MEASURED (`G25Probe`) on an `SSLServerSocket` nobody has configured.
    #[test]
    fn sss_mode_defaults_are_the_measured_hotspot_values() {
        assert_eq!(SSS_MODE_DEFAULT.0, 0, "getUseClientMode on a server socket");
        assert_eq!(SSS_MODE_DEFAULT.1, 1, "getEnableSessionCreation");
    }

    /// NOM-6. MEASURED, HotSpot 25.0.3+9 (`G25Probe`): the packet size is a
    /// constant in every state, the application size is not - 16704 before a
    /// handshake and 16676 after a TLS 1.3 one, under both AES-128-GCM and
    /// AES-256-GCM. 16384 (the old answer) is neither; it is the RFC 8446
    /// section 5.1 TLSPlaintext cap.
    #[test]
    fn the_session_buffer_sizes_are_two_states_not_one_constant() {
        assert_eq!(JSSE_PACKET_BUFFER_SIZE, 16709);
        assert_eq!(JSSE_APPLICATION_BUFFER_SIZE_FRESH, 16704);
        assert_eq!(JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED, 16676);
        assert_eq!(
            JSSE_APPLICATION_BUFFER_SIZE_FRESH,
            JSSE_PACKET_BUFFER_SIZE - 5,
            "the fresh value is the packet size less the 5-byte record header"
        );
        assert!(
            JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED < JSSE_APPLICATION_BUFFER_SIZE_FRESH,
            "negotiating a suite costs record expansion, so the app buffer shrinks"
        );
        assert_ne!(
            JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED, 16384,
            "16384 is the RFC 8446 plaintext cap, not the JDK's answer"
        );
        // The audit that made raising this safe: `do_wrap` never consumes more
        // than 16384 bytes of plaintext per call whatever the caller offers, so
        // one record can never exceed the packet buffer.
        let source = include_str!("t27_tls.rs");
        // Fragments again: a literal here would match ITSELF and the assertion
        // would hold whether or not `do_wrap` still caps.
        let cap = format!("{}{}", "bb_read_into(ctx, *bb, &mut app_bytes, ", "16384)");
        assert!(
            source.contains(&cap),
            "do_wrap's 16384 plaintext cap is what bounds one record below \
             getPacketBufferSize(); without it the larger app buffer can \
             livelock a BUFFER_OVERFLOW retry loop"
        );
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
    /// Full, hello-retry or RESUMED — see `client_session_cache`, the only
    /// consumer, which must not hand back a previous session object for a
    /// handshake that was not actually a continuation of it.
    fn handshake_kind(&self) -> Option<rustls::HandshakeKind> {
        match self {
            EngineConn::Client(c) => c.handshake_kind(),
            EngineConn::Server(s) => s.handshake_kind(),
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
    /// Queue a fatal alert for the peer — see
    /// `rustls::CommonState::queue_fatal_alert` (a CratonVM addition to the
    /// vendored fork) for why this exists and why it is idempotent.
    fn queue_fatal_alert(&mut self, desc: rustls::AlertDescription) {
        match self {
            EngineConn::Client(c) => c.queue_fatal_alert(desc),
            EngineConn::Server(s) => s.queue_fatal_alert(desc),
        }
    }
    /// Has the peer sent us a `close_notify`? The inbound half of the
    /// connection is then closed for good — see `do_unwrap`'s CLOSED report.
    fn peer_has_closed(&self) -> bool {
        match self {
            EngineConn::Client(c) => c.has_received_close_notify(),
            EngineConn::Server(s) => s.has_received_close_notify(),
        }
    }
    fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]> {
        match self {
            EngineConn::Client(c) => c.peer_certificates(),
            EngineConn::Server(s) => s.peer_certificates(),
        }
    }
}

pub(crate) struct EngineState {
    /// `None` until `beginHandshake` realizes the connection (we need
    /// client-vs-server + ALPN list known before constructing rustls).
    conn: Option<EngineConn>,
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
    conn_checked_out: bool,
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
    /// Advisory peer port recorded by `SSLContext.createSSLEngine(host, port)`.
    /// rustls never needs it (the caller owns the already-connected socket); it
    /// exists so `SSLEngine.getPeerPort()` can answer what the application asked
    /// for instead of the uninitialised `-1` of a bare synthetic allocation.
    peer_port: i32,
    /// JSSE's delegated-task state for this engine. See [`DelegatedTask`].
    delegated_task: DelegatedTask,
    /// This engine has already used its one deferral. Without it a caller that
    /// keeps handing the engine back its own NEED_TASK would re-arm forever.
    delegated_task_armed: bool,
    /// We told a caller NEED_TASK and have not yet handed it a `Runnable`.
    /// Deliberately NOT cleared when the work is done inline — see the `Owed`
    /// arm of [`engine_begin_or_defer`] for what returning `null` to a caller
    /// that was promised a task costs.
    task_unclaimed: bool,
    /// Complete TLS records taken out of the caller's buffer while the
    /// connection did not exist yet, waiting for the delegated task to realize
    /// it. See the NEED_TASK arm of `do_unwrap`.
    deferred_inbound: Vec<u8>,
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
    deferred_handshake_error: Option<(&'static str, String)>,
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
    endpoint_id_alg: Option<String>,
    /// Per-`SSLContext` (cert_pem, key_pem) copied from the context that created
    /// this engine via `createSSLEngine`. When set, `engine_begin` builds the
    /// server (or client) config from THIS identity instead of the process-
    /// global `runtime_tls_identity`, so an in-process mTLS test's server and
    /// client engines each use their own keystore.
    identity_override: Option<(String, String)>,
    /// Trust roots copied from the SSLContext that created this engine.
    trust_roots_override: Option<TlsTrustRoots>,
    /// Decrypted application bytes that did not fit the caller's `unwrap`
    /// destination buffers. Served first on the next `unwrap`. MUST be kept
    /// separate from `outbound` (encrypted TLS records) — mixing decrypted
    /// plaintext into `outbound` makes the next `wrap` emit plaintext on the
    /// wire, which the peer rejects ("corrupt message of type InvalidContentType").
    plaintext_pending: Vec<u8>,
    /// The peer's certificate chain (DER, leaf first), captured once the
    /// handshake finishes. For a server engine this is the CLIENT certificate
    /// (mTLS) — Tomcat's SSLAuthenticator reads it via
    /// `SSLSession.getPeerCertificates()` to authenticate/authorize the client.
    peer_cert_chain_der: Vec<Vec<u8>>,
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
    trust_managers_ctx_key: Option<u64>,
    /// Set once the post-handshake trust-manager consultation has run for this
    /// engine (whether it passed, failed, or found nothing to check) so it
    /// only happens once per connection.
    trust_check_done: bool,
    /// True once this engine's rustls config was built asking the peer for a
    /// certificate (NEED, WANT, or the deferred-auth substitute below). Used
    /// to tell Tomcat's "renegotiate to collect the client certificate"
    /// second `beginHandshake()` apart from an ordinary redundant one.
    client_auth_requested: bool,
    /// Set once this server engine's `SNIMatcher`s have been consulted for the
    /// ClientHello's `server_name`, so the callback into Java happens once per
    /// connection — same one-shot discipline as `trust_check_done`.
    sni_match_done: bool,
    /// `beginHandshake()` was called on a SERVER engine whose rustls connection
    /// is deliberately NOT realized yet, because a handshake ALPN selector has
    /// to see the ClientHello first (see `engine_apply_alpn_selector`). Makes
    /// `handshake_status_of` answer NEED_UNWRAP rather than NOT_HANDSHAKING for
    /// that window, so a caller that polls the status still feeds us the hello.
    alpn_selection_deferred: bool,
    /// The `legacy_session_id` seen in the ServerHello — written by the server
    /// engine as it produces that record and read by the client engine as it
    /// consumes it, so both ends of one connection hold the same bytes. See
    /// [`peek_server_hello_session_id`]; consumed by
    /// `build_synthetic_ssl_session` for TLS 1.2 sessions only.
    negotiated_session_id: Vec<u8>,
    /// For a CLIENT engine: did this side actually PRESENT a certificate?
    ///
    /// Written by [`RecordingClientCertResolver`] from inside rustls's
    /// handshake, which is the only moment the answer exists. `None` until
    /// `engine_begin` builds the client config; `Some(false)` means the
    /// resolver was installed and never returned a certificate. Read by
    /// `build_synthetic_ssl_session` to decide whether
    /// `SSLSession.getLocalCertificates()` has anything to report.
    client_cert_presented: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            conn: None,
            conn_checked_out: false,
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
            peer_port: -1,
            delegated_task: DelegatedTask::None,
            delegated_task_armed: false,
            task_unclaimed: false,
            deferred_inbound: Vec::new(),
            deferred_handshake_error: None,
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
fn engine_objref_key(ctx: &dyn NativeContext, o: ObjectRef) -> u64 {
    ctx.identity_hash_code(o) as u32 as u64
}

fn engine_id_or_alloc(ctx: &dyn NativeContext, obj: ObjectRef) -> i32 {
    let key = engine_objref_key(ctx, obj);
    let mut tab = engine_table().lock();
    if let Some(id) = tab.get(&key) {
        return *id;
    }
    let id = engine_alloc_id();
    tab.insert(key, id);
    drop(tab);
    engine_registry().write().insert(id, EngineState::default());
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
/// Fetch a REAL enum constant via the enum's generated `valueOf(String)` so the
/// returned reference is the singleton — `==` comparisons in JSSE/connector code
/// (e.g. `result.getStatus() == OK`, `engine.getHandshakeStatus() == NEED_WRAP`)
/// then work. Returns `Object(None)` if the enum can't be resolved.
fn enum_const(
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
pub(crate) fn real_status_enum(ctx: &mut dyn cratonvm_native_api::NativeContext, sr: i32) -> Value {
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
pub(crate) fn real_handshake_status_enum(
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

fn status_name(sr: i32) -> &'static str {
    match sr {
        SR_BUFFER_OVERFLOW => "BUFFER_OVERFLOW",
        SR_BUFFER_UNDERFLOW => "BUFFER_UNDERFLOW",
        SR_CLOSED => "CLOSED",
        _ => "OK",
    }
}

fn hs_name(hs: i32) -> &'static str {
    match hs {
        HS_FINISHED_R => "FINISHED",
        HS_NEED_TASK_R => "NEED_TASK",
        HS_NEED_WRAP_R => "NEED_WRAP",
        HS_NEED_UNWRAP_R => "NEED_UNWRAP",
        _ => "NOT_HANDSHAKING",
    }
}

fn alloc_engine_result(
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
fn handshake_status_of(s: &EngineState) -> i32 {
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

/// How a ByteBuffer's metadata fields were resolved — determines which
/// slot(s) `bb_set_pos` must write back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BbLayout {
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
enum BbBacking {
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
struct BbView {
    backing: BbBacking,
    layout: BbLayout,
    pos: usize,
    lim: usize,
    cap: usize,
}

fn bb_view(ctx: &mut dyn cratonvm_native_api::NativeContext, bb: ObjectRef) -> BbView {
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
fn bb_describe(
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
fn bb_get_byte(
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
fn bb_bytes_range(
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
fn bb_put_bytes(
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
fn bb_set_pos(
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
fn bb_read_into(
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
fn bb_write_from(
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
fn default_engine_client_config(alpn: &[Vec<u8>]) -> Result<Arc<ClientConfig>, String> {
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
fn default_engine_server_config(
    alpn: &[Vec<u8>],
    need_client_auth: bool,
) -> Result<Arc<ServerConfig>, String> {
    let alpn_strs: Vec<&str> = alpn
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .collect();
    let identity = runtime_tls_identity()
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
fn deferred_client_auth_contexts() -> &'static Mutex<std::collections::HashSet<u64>> {
    static T: OnceLock<Mutex<std::collections::HashSet<u64>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn mark_deferred_client_auth(ctx_key: u64) {
    deferred_client_auth_contexts().lock().insert(ctx_key);
}

fn wants_deferred_client_auth(ctx_key: Option<u64>) -> bool {
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
pub(crate) const HANDSHAKE_ERR_PREFIX: &str = "handshake_failure: ";

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
enum DelegatedTask {
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
fn handshake_error_message(e: &rustls::Error) -> String {
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

fn jsse_handshake_exception_class(e: &rustls::Error) -> &'static str {
    match e {
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
/// MUST be called with the `engine_registry()` lock DROPPED: it allocates.
fn engine_begin_failure(
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
fn claim_delegated_task(id: i32) -> bool {
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
struct UnpinOnDrop {
    base: usize,
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
struct ConnCheckout {
    id: i32,
    conn: Option<EngineConn>,
}

impl ConnCheckout {
    /// Take the connection out of the registry, marking the engine as on-loan.
    /// Answers an empty checkout (a harmless no-op on drop) when the engine has
    /// no connection yet, so callers need no special case.
    fn take(id: i32) -> Self {
        let conn = {
            let regs = engine_registry();
            let mut g = regs.write();
            match g.get_mut(&id) {
                Some(s) => {
                    let c = s.conn.take();
                    s.conn_checked_out = c.is_some();
                    c
                }
                None => None,
            }
        };
        Self { id, conn }
    }
}

impl Drop for ConnCheckout {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        let regs = engine_registry();
        let mut g = regs.write();
        if let Some(s) = g.get_mut(&self.id) {
            // Unconditional overwrite: `engine_begin` refuses to run while
            // `conn_checked_out` holds, so nothing can have installed a rival
            // connection in the window.
            s.conn = Some(conn);
            s.conn_checked_out = false;
        }
        // An engine dropped from the registry mid-loop (close on another
        // thread) simply loses the connection here, which is what closing it
        // means; there is nowhere to put it back.
    }
}

fn engine_begin_or_defer(id: i32) -> Result<bool, String> {
    let mut g = engine_registry().write();
    let Some(s) = g.get_mut(&id) else {
        return Ok(false);
    };
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
fn engine_begin_if_needed(state: &mut EngineState) -> Result<(), String> {
    if state.conn.is_some() {
        return Ok(());
    }
    engine_begin(state)
}

/// Begin the handshake — construct the rustls connection from the cached
/// configs (or defaults) and stash it on the engine.
fn engine_begin(state: &mut EngineState) -> Result<(), String> {
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
        // see `ctx_client_config_table`. A client WITH an identity is excluded
        // below (it needs a per-engine `RecordingClientCertResolver`), so the
        // identity is not part of the key.
        let shape = format!(
            "{:?}|{:?}|{:?}|{:?}|{}|{}",
            state.enabled_ciphers,
            state.enabled_protocols,
            state.alpn_protocols,
            (&state.endpoint_id_alg, &state.peer_host),
            state.need_client_auth,
            state.want_client_auth,
        );
        let shareable = state.identity_override.is_none() && state.client_config.is_none();
        let cached = if shareable {
            state.trust_managers_ctx_key.and_then(|k| {
                ctx_client_config_table()
                    .lock()
                    .get(&(k, shape.clone()))
                    .cloned()
            })
        } else {
            None
        };
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
                let endpoint_identity = match (&state.endpoint_id_alg, &state.peer_host) {
                    (Some(alg), Some(host))
                        if endpoint_alg_verifies_identity(alg)
                            && !host.is_empty()
                            && ctx_jsse_identifies(state.trust_managers_ctx_key) =>
                    {
                        Some((alg.clone(), host.clone()))
                    }
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
        let config = if needs_recorder {
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
            .peer_host
            .clone()
            .unwrap_or_else(|| "localhost".to_string());
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
                    default_engine_server_config(&state.alpn_protocols, state.need_client_auth)?
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
fn complete_tls_record_prefix(data: &[u8], capacity: usize) -> usize {
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
fn engine_wrap_pump(
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
            "[tls] BUG: wrap on an engine whose connection is checked out by              do_unwrap's record loop; the write is being dropped. Please report              this with CRATONVM_DBG=tls-hs output."
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
    (consumed, produced)
}

/// Push inbound TLS bytes into rustls, process packets, then drain plaintext
/// into the dsts (returned as `Vec<u8>`). Returns (consumed_from_src, plaintext_out).
fn engine_unwrap_pump(state: &mut EngineState, inbound: &[u8]) -> Result<(usize, Vec<u8>), String> {
    engine_unwrap_pump_raw(state, inbound).map_err(|e| format!("rustls process_new_packets: {}", e))
}

/// As [`engine_unwrap_pump`], but surfacing the rustls error itself.
///
/// The KIND of the error decides which `SSLException` subclass JSSE raises
/// (`jsse_handshake_exception_class`), and a stringified error cannot be
/// classified: `testTruncatedPacket` wants `SSLProtocolException` for a
/// ServerHello pushed into a server engine and sees `SSLHandshakeException`
/// if the class is picked before the kind is known.
fn engine_unwrap_pump_raw(
    state: &mut EngineState,
    inbound: &[u8],
) -> Result<(usize, Vec<u8>), rustls::Error> {
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
struct PendingTrustCheck {
    /// The engine this check belongs to, so a rejection can queue the fatal
    /// alert JSSE sends the peer — see `reject_peer_with_fatal_alert`.
    engine_id: i32,
    is_client: bool,
    peer_chain_der: Vec<Vec<u8>>,
    /// `None` when the owning `SSLContext` has no `TrustManager[]` attached —
    /// the pending check can still be worth running for its endpoint-identity
    /// half, which JSSE performs regardless of which TrustManager is in force.
    trust_ctx_key: Option<u64>,
    negotiated_cipher_suite_name: Option<String>,
    /// `(algorithm, peer_host)` when this client engine must perform RFC 2818 /
    /// RFC 6125 endpoint identification once the chain is accepted.
    endpoint_identity: Option<(String, String)>,
}

/// True for the `SSLParameters.setEndpointIdentificationAlgorithm()` values
/// that mean "verify the peer certificate names the host I dialled".
///
/// JSSE recognises `HTTPS` (RFC 2818) and `LDAPS` (RFC 2830); the comparison is
/// case-insensitive there, and anything else — including the empty string and
/// `null` — means no identification. We deliberately do NOT treat an unknown
/// algorithm as "verify anyway": that would reject connections JSSE accepts.
fn endpoint_alg_verifies_identity(alg: &str) -> bool {
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
fn engine_pending_sni_host(
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

fn engine_take_pending_trust_check(id: i32, state: &mut EngineState) -> Option<PendingTrustCheck> {
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
    let endpoint_identity = match (&state.endpoint_id_alg, &state.peer_host) {
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
fn peek_client_hello_sni(buf: &[u8]) -> Option<String> {
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
        p += 4 + ext_len;
    }
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
fn peek_client_hello_alpn(buf: &[u8]) -> Option<Vec<String>> {
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
                    out.push(s.to_string());
                }
                q += 1 + n;
            }
            return Some(out);
        }
        p += 4 + ext_len;
    }
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
fn peek_server_hello_session_id(buf: &[u8]) -> Option<Vec<u8>> {
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
fn engine_alpn_selector_table(
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
fn engine_apply_alpn_selector(
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
fn capture_sni_matchers(ctx: &mut dyn NativeContext, engine: ObjectRef, params: ObjectRef) {
    let key = engine_objref_key(ctx, engine);
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
                Ok(Some(Value::Object(Some(m)))) => list.push(m),
                _ => break,
            }
        }
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
fn engine_run_sni_match_check(
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
    let name_str = ctx.create_string(&host);
    let base = ctx.pin_native_root(name_str);
    let name_str = ctx.read_native_pin(base, name_str);
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
    let m_pins: Vec<usize> = matchers.iter().map(|m| ctx.pin_native_root(*m)).collect();
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
    static IN_TRUST_CHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// As [`IN_TRUST_CHECK`], but true only for the CLIENT-side callback
    /// (`checkServerTrusted`). See [`in_client_trust_check`].
    static IN_CLIENT_TRUST_CHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Is this thread inside an application `TrustManager` callback? See
/// [`IN_TRUST_CHECK`].
fn in_trust_check() -> bool {
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
pub(crate) fn in_client_trust_check() -> bool {
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
enum TrustCheckMode {
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
enum TrustOutcome {
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
}

fn engine_run_trust_check(
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
    }
}

fn engine_consult_trust_managers(
    ctx: &mut dyn NativeContext,
    pending: PendingTrustCheck,
    engine_obj: Option<ObjectRef>,
    mode: TrustCheckMode,
) -> Result<TrustOutcome, cratonvm_types::error::MethodCallFailed> {
    let trust_managers = match pending.trust_ctx_key {
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
    let mut arr = arr;
    for (i, der) in pending.peer_chain_der.iter().enumerate() {
        let mirror = crate::keystore::make_x509_mirror(ctx, "peer", der)?;
        // No allocation between this re-read and the store.
        arr = ctx.read_native_pin(base, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    let auth_type_str = ctx.create_string(auth_type);

    // Pin the authType string and every TrustManager we're about to call —
    // each `invoke_virtual` below can allocate/GC, and a stale ObjectRef from
    // an earlier loop iteration would silently resolve to a reused slot after
    // a move (see `pin_native_root`'s doc). Mirrors the existing multi-call
    // pin pattern in `net_phase_e.rs`'s group-collector native. `base + 1` is
    // the authType pin because these two pins are taken back to back.
    let _ = ctx.pin_native_root(auth_type_str);
    // The `SSLEngine` goes into the same pin scope: the three-argument
    // `checkServerTrusted` overload passes it to Java, and every
    // `invoke_virtual` in the loop below can move it.
    let engine_pin = engine_obj.map(|e| (ctx.pin_native_root(e), e));
    let tm_pins: Vec<usize> = trust_managers
        .iter()
        .map(|tm| ctx.pin_native_root(*tm))
        .collect();

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
                    ctx.unpin_native_roots(base);
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
            rejection = Some(match &e {
                cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc) => {
                    let cls = ctx
                        .class_name_of_id(ctx.class_id_of_object(*exc))
                        .unwrap_or_else(|| "<unknown class>".to_string());
                    match ctx.invoke_virtual(*exc, "getMessage", "()Ljava/lang/String;", &[]) {
                        Ok(Some(Value::Object(Some(s)))) => {
                            format!("{cls}: {}", ctx.read_string(s).unwrap_or_default())
                        }
                        _ => cls,
                    }
                }
                other => format!("{other:?}"),
            });
            // Which alert this becomes is decided HERE, while the exception is
            // still an object and Java can still be called. Both consumers run
            // somewhere that cannot do either: `reject_peer_with_fatal_alert`
            // is on the unwinding path, and the verifier-time arm is inside
            // rustls's state machine.
            rejection_alert = match &e {
                cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc) => {
                    crate::tls_cert_alert::jsse_cert_alert_for(ctx, *exc)
                }
                // Not an exception at all (an internal error): JSSE has no
                // opinion, and `certificate_unknown` is its default.
                _ => crate::tls_cert_alert::JsseCertAlert::CertificateUnknown,
            };
            rejected = true;
            break;
        }
    }
    IN_TRUST_CHECK.with(|c| c.set(false));
    IN_CLIENT_TRUST_CHECK.with(|c| c.set(false));
    ctx.unpin_native_roots(base);

    if rejected {
        let detail = rejection.unwrap_or_else(|| "no exception detail available".to_string());
        set_last_trust_rejection_detail(&detail);
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
fn reject_peer_with_fatal_alert(engine_id: i32, alert: crate::tls_cert_alert::JsseCertAlert) {
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
fn tm_is_extended(ctx: &mut dyn NativeContext, tm: ObjectRef) -> bool {
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
fn throwable_is_error(ctx: &mut dyn NativeContext, exc: ObjectRef) -> bool {
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

fn jsse_owns_endpoint_identification(
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
fn engine_check_endpoint_identity(
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
    static LAST_TRUST_REJECTION: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

fn set_last_trust_rejection_detail(detail: &str) {
    LAST_TRUST_REJECTION.with(|c| *c.borrow_mut() = Some(detail.to_string()));
}

/// Consume the reason recorded by the most recent TrustManager rejection on
/// this thread, if any. See [`LAST_TRUST_REJECTION`].
pub(crate) fn take_last_trust_rejection_detail() -> Option<String> {
    LAST_TRUST_REJECTION.with(|c| c.borrow_mut().take())
}

/// Client-socket variant of the post-handshake TrustManager consultation:
/// run the attached Java TrustManagers' `checkServerTrusted` against the
/// peer chain captured by a native client connect
/// (`servlet::s2_tls_connect`). Same rejection semantics as
/// `engine_run_trust_check` (any thrown exception → `SSLHandshakeException`).
pub(crate) fn run_client_trust_check_for_chain(
    ctx: &mut dyn NativeContext,
    trust_ctx_key: u64,
    peer_chain_der: Vec<Vec<u8>>,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    engine_run_trust_check(
        ctx,
        PendingTrustCheck {
            // No SSLEngine here: this is the native client-socket path, whose
            // rustls connection is owned by `servlet::s2_tls_connect` and is
            // not in `engine_registry`. `reject_peer_with_fatal_alert` is a
            // no-op for an id that names no engine, which is the right answer
            // — that path tears the socket down itself.
            engine_id: -1,
            is_client: true,
            peer_chain_der,
            trust_ctx_key: Some(trust_ctx_key),
            negotiated_cipher_suite_name: None,
            // The native client-socket path performs its own hostname check at
            // its own layer (`http_url_connection::huc_verify_hostname`); it
            // does not route endpoint identification through here.
            endpoint_identity: None,
        },
        None,
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
enum SessionPhase {
    Fresh,
    Handshaking,
    Negotiated,
}

/// Which method the caller came through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SessionDoor {
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
fn session_phase(door: SessionDoor, handshaked: bool) -> SessionPhase {
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

#[cfg(test)]
mod session_phase_tests {
    use super::{session_phase, SessionDoor, SessionPhase};

    /// The distinction that was missing.
    #[test]
    fn the_null_session_is_not_the_pending_session() {
        assert_ne!(
            session_phase(SessionDoor::Current, false),
            session_phase(SessionDoor::Handshake, false),
        );
    }

    /// …and the pending session is what `getSession()` answers afterwards, so
    /// a mid-handshake `putValue` survives — `mustCallResumeTrustedOnSession
    /// Resumption`, which HUNG when it did not. Asserted as the two phases the
    /// carry rule joins, since they are deliberately distinct KEYS and
    /// `engine_session_for` copies the bindings across.
    #[test]
    fn the_negotiated_session_is_the_one_that_was_pending() {
        assert_eq!(
            session_phase(SessionDoor::Current, true),
            SessionPhase::Negotiated
        );
        assert_eq!(
            session_phase(SessionDoor::Handshake, false),
            SessionPhase::Handshaking
        );
    }

    /// `getSession()` while a handshake is in flight is still the NULL
    /// session, not the pending one.
    #[test]
    fn get_session_mid_handshake_is_still_the_null_session() {
        assert_eq!(
            session_phase(SessionDoor::Current, false),
            SessionPhase::Fresh
        );
    }
}

fn engine_session_table(
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
fn client_session_cache(
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
fn client_session_cache_key(id: i32) -> Option<(u64, String, i32)> {
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
fn session_last_accessed_table(
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

fn touch_session_access_time(ctx: &mut dyn NativeContext, ses: ObjectRef) {
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
fn engine_handshake_was_resumed(id: i32) -> bool {
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
fn negotiated_session_keys(
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
fn session_is_negotiated(ctx: &mut dyn NativeContext, ses: ObjectRef) -> bool {
    let key = gc_stable_objref_key(ctx, ses);
    negotiated_session_keys().lock().contains(&key)
}

/// `getSession()`'s stable answer: the cached session for this engine's current
/// handshake epoch, built on first use. See [`engine_session_table`].
fn engine_session_for(
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
            let attrs_slot = ctx.object_num_fields(pending) - 1;
            if let Value::Object(Some(attrs)) = ctx.get_field(pending, attrs_slot) {
                // Only when the pending session actually has a map — allocating
                // one here would hand every negotiated session a non-null slot
                // it did not have before.
                let ses_slot = ctx.object_num_fields(ses) - 1;
                ctx.set_field(ses, ses_slot, Value::Object(Some(attrs)));
            }
        }
        let k = gc_stable_objref_key(ctx, ses);
        negotiated_session_keys().lock().insert(k);
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
fn build_synthetic_ssl_session(
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
        // docs/known-issues/jdk-only/
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
    let ses = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 8)?;
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
    let ses = ctx.read_native_pin(pin, ses);
    let cipher_s = ctx.read_native_pin(cipher_pin, cipher_s);
    let proto_s = ctx.read_native_pin(proto_pin, proto_s);
    ctx.set_field(ses, 0, Value::Object(Some(cipher_s)));
    ctx.set_field(ses, 1, Value::Object(Some(proto_s)));
    ctx.set_field(ses, 2, Value::Int(if negotiated { 1 } else { 0 }));
    ctx.set_field(ses, 3, Value::Object(None));
    ctx.set_field(ses, 4, Value::Int(-1));
    ctx.set_field(ses, 5, Value::Long(crate::epoch_millis_now()));
    ctx.set_field(ses, 6, Value::Object(Some(alpn_s)));
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
        session_peer_certs_table()
            .lock()
            .insert(gc_stable_objref_key(ctx, ses), peer_chain);
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
                    .then(runtime_tls_identity)
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
fn engine_peer_host_text(ctx: &mut dyn NativeContext, engine: ObjectRef) -> String {
    match ctx.invoke_virtual(engine, "getPeerHost", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_else(|| "null".into()),
        _ => "null".to_string(),
    }
}

/// `getPeerPort()`. A CratonVM engine that never had a host recorded reads the
/// field, which `createSSLEngine()` seeds to -1 — the value
/// `javax.net.ssl.SSLEngine`'s own field initialiser uses, and the one HotSpot
/// prints.
fn engine_peer_port_text(ctx: &mut dyn NativeContext, engine: ObjectRef) -> String {
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
fn engine_session_text(ctx: &mut dyn NativeContext, engine: ObjectRef) -> String {
    let session =
        match ctx.invoke_virtual(engine, "getSession", "()Ljavax/net/ssl/SSLSession;", &[]) {
            Ok(Some(Value::Object(Some(sess)))) => sess,
            _ => return "null".to_string(),
        };
    match ctx.invoke_virtual(session, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_else(|| "null".into()),
        _ => "null".to_string(),
    }
}

fn register_engine_impl_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    });

    // Constructor — allocates an engine_id slot in the side-table.
    r.register(cls_impl, "<init>", "()V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let _ = engine_id_or_alloc(ctx, *this);
        }
        Ok(None)
    });

    // setUseClientMode(Z)V
    r.register(cls_impl, "setUseClientMode", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        let id = engine_id_or_alloc(ctx, this);
        with_engine(id, |s| {
            s.is_client = mode != 0;
        });
        Ok(None)
    });

    r.register(cls_impl, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        let mode = with_engine(id, |s| s.is_client).unwrap_or(true);
        Ok(Some(Value::Int(if mode { 1 } else { 0 })))
    });

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
            match args.get(1) {
                Some(Value::Object(Some(f))) => {
                    engine_alpn_selector_table().lock().insert(key, *f);
                }
                // An explicit null CLEARS the selector, per `SSLEngine`'s
                // contract ("null to remove").
                _ => {
                    engine_alpn_selector_table().lock().remove(&key);
                }
            }
            Ok(None)
        },
    );
    // Now a truthful echo of what the setter stored: netty reads this pair to
    // decide whether JDK-style ALPN callbacks are live, and they are.
    r.register(
        cls_impl,
        "getHandshakeApplicationProtocolSelector",
        "()Ljava/util/function/BiFunction;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = engine_objref_key(ctx, this);
            let f = engine_alpn_selector_table().lock().get(&key).copied();
            Ok(Some(Value::Object(f)))
        },
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
                s.want_client_auth = false;
            }
        });
        Ok(None)
    });

    r.register(cls_impl, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        let v = with_engine(id, |s| s.need_client_auth).unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    });

    r.register(cls_impl, "setWantClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|x| x.as_int()).unwrap_or(0) != 0;
        let id = engine_id_or_alloc(ctx, this);
        with_engine(id, |s| {
            s.want_client_auth = v;
            if v {
                s.need_client_auth = false;
            }
        });
        Ok(None)
    });

    r.register(cls_impl, "getWantClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
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
            let id = engine_id_or_alloc(ctx, this);
            let list = with_engine(id, |s| s.enabled_protocols.clone())
                .unwrap_or_else(|| vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()]);
            // GC NOTE: `create_string` allocates, so the array is rooted
            // across the loop — see `x509_manager::materialize_string_array`.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let arr = scope.new_ref_array(cratonvm_types::ClassId::new(0), list.len());
            let arr_h = scope.root(arr);
            for (i, p) in list.iter().enumerate() {
                let s = scope.create_string(p);
                let arr = scope.get(&arr_h);
                scope.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
        },
    );

    r.register(
        cls_impl,
        "getSupportedProtocols",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // JSSE exposes TLSv1.1 as a configurable legacy protocol even
            // though the rustls transport below cannot negotiate it. Tomcat
            // intersects SSLHostConfig.protocols with this advertised set
            // before it stores the connector configuration; omitting it here
            // therefore destroys an explicit `TLSv1.1+TLSv1.2` configuration
            // instead of preserving its requested policy. The handshake
            // mapper remains intentionally limited to rustls's TLS 1.2/1.3
            // implementation, so this only restores the configuration API's
            // round-trip contract.
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 3);
            let s1 = ctx.create_string("TLSv1.3");
            let s2 = ctx.create_string("TLSv1.2");
            let s3 = ctx.create_string("TLSv1.1");
            ctx.set_array_element(arr, 0, Value::Object(Some(s1)));
            ctx.set_array_element(arr, 1, Value::Object(Some(s2)));
            ctx.set_array_element(arr, 2, Value::Object(Some(s3)));
            Ok(Some(Value::Object(Some(arr))))
        },
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
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), suites.len());
            for (i, &s) in suites.iter().enumerate() {
                let so = ctx.create_string(s);
                ctx.set_array_element(arr, i, Value::Object(Some(so)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
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
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), names.len());
            for (i, p) in names.iter().enumerate() {
                let s = ctx.create_string(p);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
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
        let res = {
            let mut g = engine_registry().write();
            match g.get_mut(&id) {
                Some(s) => engine_begin(s),
                None => Ok(()),
            }
        };
        if let Err(e) = res {
            return Err(engine_begin_failure(ctx, e));
        }
        Ok(None)
    });

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
        },
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
        let outcome = {
            let mut g = engine_registry().write();
            match g.get_mut(&id) {
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
                                match engine_unwrap_pump_raw(s, &staged) {
                                    Ok((_, pt)) => {
                                        // Any plaintext belongs to the caller's
                                        // next unwrap; `do_unwrap`'s Step 0
                                        // serves this buffer before it touches
                                        // the network again.
                                        s.plaintext_pending.extend_from_slice(&pt);
                                        // NOT `engine_capture_negotiation` —
                                        // the task runs mid-handshake, and
                                        // capturing there latched a session id
                                        // that was not the negotiated one:
                                        // `SSLEngineTest.testSSLSessionId`
                                        // read "array contents differ at index
                                        // [0]" on all six of its running
                                        // parameterisations. `do_wrap` and
                                        // `do_unwrap` both capture at the
                                        // right moment already.
                                        Ok(None)
                                    }
                                    Err(e) => {
                                        // Handed to the engine, not thrown.
                                        // See the note above `r.register(
                                        // "java/lang/Runnable", ...)`.
                                        s.deferred_handshake_error = Some((
                                            jsse_handshake_exception_class(&e),
                                            format!("rustls: {}", e),
                                        ));
                                        Ok(Some(format!("rustls: {}", e)))
                                    }
                                }
                            }
                        }
                    }
                }
            }
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
            let id = engine_id_or_alloc(ctx, this);
            let hs = with_engine(id, |s| handshake_status_of(s)).unwrap_or(HS_NOT_HANDSHAKING_R);
            // Return the REAL enum singleton so `engine.getHandshakeStatus() ==
            // NEED_WRAP` etc. in the connector's handshake loop work.
            Ok(Some(real_handshake_status_enum(ctx, hs)))
        },
    );

    r.register(cls_impl, "closeOutbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
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
                c.send_close_notify();
            }
        });
        Ok(None)
    });

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
    });

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
    });

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
    });

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
        },
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
        },
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
        },
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
            let id = engine_id_or_alloc(ctx, this);
            let alpn = with_engine(id, |s| s.negotiated_alpn.clone())
                .flatten()
                .unwrap_or_default();
            let s = ctx.create_string(&alpn);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.set_category(__prev_cat);
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
            )?))))
        }
    };
    Ok(do_wrap(ctx, this, src.into_iter().collect(), dst)?)
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

fn do_wrap(
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

    {
        // The lock is dropped before the failure is reported: `engine_begin_failure`
        // allocates the Java exception, and allocating under `engine_registry()`'s
        // write lock is the GC self-deadlock `EngineState::trust_managers_ctx_key`
        // documents.
        let res = {
            let mut g = engine_registry().write();
            match g.get_mut(&id) {
                Some(s) if s.conn.is_none() => engine_begin(s),
                _ => Ok(()),
            }
        };
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
            )?))))
        }
    };
    let dst = match args.get(2) {
        Some(Value::Object(Some(b))) => Some(*b),
        _ => None,
    };
    Ok(do_unwrap(ctx, this, src, dst.into_iter().collect())?)
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

fn do_unwrap(
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

    // JSSE's server-side SNI gate, run at ClientHello time — see
    // `peek_client_hello_sni` for why it cannot wait until rustls has parsed
    // the record. Nothing is consumed here; on refusal the bytes are never fed
    // to rustls at all, so no ServerHello is ever produced.
    if let Some(host) = engine_pending_sni_host(ctx, id, &src_view, src_pos, src_lim) {
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
        let mut g = engine_registry().write();
        match g.get_mut(&id) {
            Some(s) => engine_begin_if_needed(s).map(|()| false),
            None => Ok(false),
        }
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
        let mut g = engine_registry().write();
        let s = match g.get_mut(&id) {
            Some(s) => s,
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
                Ok((_, pt)) => plaintext.extend_from_slice(&pt),
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
                        return Err(crate::phases_early::throw_jca_exc(
                            ctx,
                            cls,
                            &format!("rustls: {}", e),
                        ));
                    } else {
                        let msg = match e {
                            rustls::Error::AlertReceived(desc) => {
                                format!("Received fatal alert: {desc:?}")
                            }
                            other => format!("rustls process_new_packets: {other}"),
                        };
                        return Err(crate::phases_early::throw_jca_exc(
                            ctx,
                            "javax/net/ssl/SSLException",
                            &msg,
                        ));
                    }
                }
            }
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
                    let msg = match e {
                        rustls::Error::AlertReceived(desc) => {
                            // JSSE's wording, so a caller matching on the
                            // message sees what it sees on HotSpot.
                            format!("Received fatal alert: {desc:?}")
                        }
                        other => format!("rustls process_new_packets: {other}"),
                    };
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "javax/net/ssl/SSLException",
                        &msg,
                    ));
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
        let mut g = engine_registry().write();
        let s = match g.get_mut(&id) {
            Some(s) => s,
            None => {
                return Err(RuntimeError::IOException {
                    message: "engine handle missing".into(),
                }
                .into());
            }
        };
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

fn register_alpn_on_parameters(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
            sslparams_alpn_table()
                .lock()
                .insert(engine_objref_key(ctx, this), list);
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
                .get(&engine_objref_key(ctx, this))
                .cloned()
                // A fresh `SSLParameters` advertises NOTHING: HotSpot's
                // `getApplicationProtocols()` on one nobody has configured is
                // a zero-length array, not this VM's invented `[h2,
                // http/1.1]`. A caller that reads the list to decide whether
                // ALPN was requested was told yes by every parameters object
                // in the VM.
                .unwrap_or_default();
            // GC NOTE: `create_string` allocates, so the array is rooted
            // across the loop — see `x509_manager::materialize_string_array`.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let arr = scope.new_ref_array(cratonvm_types::ClassId::new(0), list.len());
            let arr_h = scope.root(arr);
            for (i, p) in list.iter().enumerate() {
                let s = scope.create_string(p);
                let arr = scope.get(&arr_h);
                scope.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
        },
    );
    r.set_category(__prev_cat);
}

fn register_apply_parameters(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // SSLEngine.setSSLParameters propagates the ALPN list onto the engine.
    r.register(
        "sun/security/ssl/SSLEngineImpl",
        "setSSLParameters",
        "(Ljavax/net/ssl/SSLParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            if let Some(Value::Object(Some(p))) = args.get(1) {
                if let Some(list) = sslparams_alpn_table()
                    .lock()
                    .get(&engine_objref_key(ctx, *p))
                    .cloned()
                {
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
                let need = ctx
                    .get_field_by_name(*p, "needClientAuth")
                    .as_int()
                    .unwrap_or(0)
                    != 0;
                let want = ctx
                    .get_field_by_name(*p, "wantClientAuth")
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
                    *p,
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
                if let Ok(Some(Value::Object(Some(list)))) =
                    ctx.invoke_virtual(*p, "getServerNames", "()Ljava/util/List;", &[])
                {
                    let n = ctx
                        .invoke_virtual(list, "size", "()I", &[])
                        .ok()
                        .flatten()
                        .and_then(|v| v.as_int())
                        .unwrap_or(0);
                    for i in 0..n.min(16) {
                        let Ok(Some(Value::Object(Some(sn)))) = ctx.invoke_virtual(
                            list,
                            "get",
                            "(I)Ljava/lang/Object;",
                            &[Value::Int(i)],
                        ) else {
                            break;
                        };
                        // Only `SNI_HOST_NAME` (type 0) names a host.
                        if !matches!(
                            ctx.invoke_virtual(sn, "getType", "()I", &[]),
                            Ok(Some(Value::Int(0)))
                        ) {
                            continue;
                        }
                        if let Ok(Some(Value::Object(Some(s)))) =
                            ctx.invoke_virtual(sn, "getAsciiName", "()Ljava/lang/String;", &[])
                        {
                            if let Some(host) = ctx.read_string(s) {
                                if !host.is_empty() {
                                    with_engine(id, |st| {
                                        st.peer_host = Some(host.clone());
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
                // SNI matchers — the server-side gate. See
                // `engine_run_sni_match_check`.
                capture_sni_matchers(ctx, this, *p);
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
            let mk = |ctx: &mut dyn cratonvm_native_api::NativeContext, items: &[String]| {
                let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), items.len());
                for (i, s) in items.iter().enumerate() {
                    let so = ctx.create_string(s);
                    ctx.set_array_element(arr, i, Value::Object(Some(so)));
                }
                Value::Object(Some(arr))
            };
            let carr = mk(ctx, &ciphers);
            let parr = mk(ctx, &protocols);
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
                .insert(engine_objref_key(ctx, p), alpn_list);
            Ok(Some(Value::Object(Some(p))))
        },
    );
    r.set_category(__prev_cat);
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
/// Copy an `SSLContext`'s per-context identity onto the engine `createSSLEngine`
/// just produced, so `engine_begin` uses this engine's own keystore cert/key
/// (server cert, or client cert for mTLS) instead of the process-global slot.
pub(crate) fn set_engine_identity_override(
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
pub(crate) fn set_engine_peer_host(
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
    let pin = ctx.pin_native_root(engine_obj);
    let host_str = ctx.create_string(&host);
    let engine_now = ctx.read_native_pin(pin, engine_obj);
    ctx.set_field_by_name(engine_now, "peerHost", Value::Object(Some(host_str)));
    ctx.set_field_by_name(engine_now, "peerPort", Value::Int(port));
    ctx.unpin_native_roots(pin);
}

/// Copy trust roots selected by the creating SSLContext even when it has no
/// identity. Pure client contexts otherwise fall back to platform roots when
/// their engine begins the handshake.
pub(crate) fn set_engine_trust_roots_override(ctx: &dyn NativeContext, engine_obj: ObjectRef) {
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
pub(crate) fn set_engine_trust_ctx_key(
    ctx: &mut dyn NativeContext,
    engine_obj: ObjectRef,
    ctx_obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let id = engine_id_or_alloc(ctx, engine_obj);
    if crate::nbflags().dbg_tls_auth_ok {
        let has_entry = ctx_trust_managers_table().lock().contains_key(&key);
        eprintln!(
            "[dbg-tls-auth] set_engine_trust_ctx_key engine_id={} ctx_key={} table_has_entry={}",
            id, key, has_entry
        );
    }
    with_engine(id, |s| {
        s.trust_managers_ctx_key = Some(key);
    });
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
fn engine_sni_matchers_table(
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
pub fn gc_scan_tls_ctx_trust_manager_roots(roots: &mut Vec<ObjectRef>) {
    let table = ctx_trust_managers_table().lock();
    for list in table.values() {
        for tm in list {
            if !tm.as_ptr().is_null() {
                roots.push(*tm);
            }
        }
    }
    drop(table);
    let matchers = engine_sni_matchers_table().lock();
    for list in matchers.values() {
        for m in list {
            if !m.as_ptr().is_null() {
                roots.push(*m);
            }
        }
    }
    drop(matchers);
    let selectors = engine_alpn_selector_table().lock();
    for f in selectors.values() {
        if !f.as_ptr().is_null() {
            roots.push(*f);
        }
    }
    drop(selectors);
    let sessions = engine_session_table().lock();
    for ses in sessions.values() {
        if !ses.as_ptr().is_null() {
            roots.push(*ses);
        }
    }
    drop(sessions);
    // The client session cache outlives the engine that created each entry —
    // that is its whole point — so it is an independent root, not something
    // `engine_session_table` keeps alive for it.
    let cached = client_session_cache().lock();
    for ses in cached.values() {
        if !ses.as_ptr().is_null() {
            roots.push(*ses);
        }
    }
    drop(cached);
    if let Some(f) = *huc_default_factory_slot().lock() {
        if !f.as_ptr().is_null() {
            roots.push(f);
        }
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
fn huc_default_factory_slot() -> &'static Mutex<Option<ObjectRef>> {
    static T: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(None))
}

pub(crate) fn set_huc_default_ssl_socket_factory(factory: ObjectRef) {
    *huc_default_factory_slot().lock() = Some(factory);
}

pub(crate) fn huc_default_ssl_socket_factory() -> Option<ObjectRef> {
    *huc_default_factory_slot().lock()
}

/// Post-move remap companion to `gc_scan_tls_ctx_trust_manager_roots`.
pub fn gc_update_tls_ctx_trust_manager_refs(map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let mut table = ctx_trust_managers_table().lock();
    for list in table.values_mut() {
        for tm in list.iter_mut() {
            let old = tm.as_ptr() as usize;
            if let Some(&new) = map.get(&old) {
                debug_assert!(new != 0, "GC pointer map contains null address");
                // SAFETY: `new` is a live, 8-byte-aligned heap address produced
                // by the moving collector for the object previously at `old`.
                *tm = unsafe { ObjectRef::from_raw(new as *mut u8) };
            }
        }
    }
    drop(table);
    let mut matchers = engine_sni_matchers_table().lock();
    for list in matchers.values_mut() {
        for m in list.iter_mut() {
            let old = m.as_ptr() as usize;
            if let Some(&new) = map.get(&old) {
                debug_assert!(new != 0, "GC pointer map contains null address");
                // SAFETY: as above.
                *m = unsafe { ObjectRef::from_raw(new as *mut u8) };
            }
        }
    }
    drop(matchers);
    let mut selectors = engine_alpn_selector_table().lock();
    for f in selectors.values_mut() {
        let old = f.as_ptr() as usize;
        if let Some(&new) = map.get(&old) {
            debug_assert!(new != 0, "GC pointer map contains null address");
            // SAFETY: as above.
            *f = unsafe { ObjectRef::from_raw(new as *mut u8) };
        }
    }
    drop(selectors);
    let mut sessions = engine_session_table().lock();
    for ses in sessions.values_mut() {
        let old = ses.as_ptr() as usize;
        if let Some(&new) = map.get(&old) {
            debug_assert!(new != 0, "GC pointer map contains null address");
            // SAFETY: as above.
            *ses = unsafe { ObjectRef::from_raw(new as *mut u8) };
        }
    }
    drop(sessions);
    let mut cached = client_session_cache().lock();
    for ses in cached.values_mut() {
        let old = ses.as_ptr() as usize;
        if let Some(&new) = map.get(&old) {
            debug_assert!(new != 0, "GC pointer map contains null address");
            // SAFETY: as above.
            *ses = unsafe { ObjectRef::from_raw(new as *mut u8) };
        }
    }
    drop(cached);
    // Same treatment for the installed default `SSLSocketFactory` — see
    // `huc_default_factory_slot`.
    let mut slot = huc_default_factory_slot().lock();
    if let Some(f) = *slot {
        let old = f.as_ptr() as usize;
        if let Some(&new) = map.get(&old) {
            debug_assert!(new != 0, "GC pointer map contains null address");
            // SAFETY: as above.
            *slot = Some(unsafe { ObjectRef::from_raw(new as *mut u8) });
        }
    }
}

/// GC root scan for `ctx_key_managers_table` — mirrors
/// `gc_scan_tls_ctx_trust_manager_roots` exactly (see that table's doc).
pub fn gc_scan_tls_ctx_key_manager_roots(roots: &mut Vec<ObjectRef>) {
    let table = ctx_key_managers_table().lock();
    for list in table.values() {
        for km in list {
            if !km.as_ptr().is_null() {
                roots.push(*km);
            }
        }
    }
}

/// Post-move remap companion to `gc_scan_tls_ctx_key_manager_roots`.
pub fn gc_update_tls_ctx_key_manager_refs(map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let mut table = ctx_key_managers_table().lock();
    for list in table.values_mut() {
        for km in list.iter_mut() {
            let old = km.as_ptr() as usize;
            if let Some(&new) = map.get(&old) {
                debug_assert!(new != 0, "GC pointer map contains null address");
                // SAFETY: `new` is a live, 8-byte-aligned heap address produced
                // by the moving collector for the object previously at `old`.
                *km = unsafe { ObjectRef::from_raw(new as *mut u8) };
            }
        }
    }
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
fn default_ssl_context_slot() -> &'static Mutex<Option<ObjectRef>> {
    static T: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(None))
}

/// `SSLContext.setDefault(SSLContext)` (native registration lives in
/// `net_phase_e.rs`, alongside `getDefault`) -- install `ctx_obj` as the
/// process-wide default so a subsequent `SSLContext.getDefault()` returns
/// this SAME object. See `default_ssl_context_slot`'s doc for why identity,
/// not a copy, is what makes this work.
pub(crate) fn set_runtime_default_ssl_context(ctx_obj: ObjectRef) {
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] set_runtime_default_ssl_context ptr={:p}",
            ctx_obj.as_ptr()
        );
    }
    *default_ssl_context_slot().lock() = Some(ctx_obj);
}

/// The `SSLContext` most recently installed via `setDefault`, if any --
/// consulted by `SSLContext.getDefault()` so it returns the caller-configured
/// object instead of always allocating a fresh, unconfigured one.
pub(crate) fn get_runtime_default_ssl_context() -> Option<ObjectRef> {
    *default_ssl_context_slot().lock()
}

/// The process-wide default `SSLContext`, created and cached on first use.
///
/// This is the JDK's documented `SSLContext.getDefault()` lazy-init contract
/// ("the default context is created if it is not yet created"). It lived
/// inline in exactly one caller — `phases_late::ssl_security`'s
/// `SSLSocketFactory.getDefault()` registration — while three other natives
/// that also hand back a `javax/net/ssl/SSLSocketFactory` minted a *bare*
/// carrier instead. The layered
/// `SSLSocketFactory.createSocket(Socket,String,int,boolean)` overload reads
/// the owning context out of the carrier's field 0, so every one of those
/// bare factories threw
/// `IllegalStateException: SSLSocketFactory has no owning SSLContext`
/// instead of connecting. Converting the idiom rather than each site is what
/// keeps a future fourth caller from re-introducing it. See
/// `sslsocketfactory-getdefault-aether-resolution-regression-20260804-FIXED.md`.
pub(crate) fn default_ssl_context_or_create(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(existing) = get_runtime_default_ssl_context() {
        return Ok(existing);
    }
    let new_ctx = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 2)?;
    let name = ctx.create_string("TLS");
    ctx.set_field(new_ctx, 0, Value::Object(Some(name)));
    ctx.set_field(new_ctx, 1, Value::Int(1));
    set_runtime_default_ssl_context(new_ctx);
    Ok(new_ctx)
}

/// Mint the object `SSLSocketFactory.getDefault()` hands back: the same
/// 1-slot synthetic carrier `SSLContext.getSocketFactory()` returns, with
/// field 0 set to the process default `SSLContext`.
///
/// `HttpsURLConnection`'s default/instance factory getters use it too — the
/// JDK documents both as defaulting to `SSLSocketFactory.getDefault()`.
pub(crate) fn default_ssl_socket_factory_obj(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    // Deliberately a FRESH carrier per call, not a cached singleton.
    // Measured on real JDK 21: `SSLSocketFactory.getDefault()` hands back a
    // different object each time (`SSLContextImpl.engineGetSocketFactory`
    // news up an `SSLSocketFactoryImpl` per call). Caching here was tried and
    // reverted — it diverges from the JDK, and the stability that callers do
    // observe belongs one layer up, in
    // `HttpsURLConnection.getDefaultSSLSocketFactory`, which caches its result
    // in its own static field (see that registration).
    let ssl_ctx = default_ssl_context_or_create(ctx);
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 1)?;
    ctx.set_field(obj, 0, Value::Object(Some(ssl_ctx?)));
    Ok(obj)
}

/// GC root scan for `default_ssl_context_slot` -- mirrors
/// `gc_scan_tls_ctx_trust_manager_roots` exactly (see that table's doc for
/// why a raw `ObjectRef` held outside the Java heap needs this).
pub fn gc_scan_default_ssl_context_root(roots: &mut Vec<ObjectRef>) {
    if let Some(ctx_obj) = *default_ssl_context_slot().lock() {
        if !ctx_obj.as_ptr().is_null() {
            roots.push(ctx_obj);
        }
    }
}

/// Post-move remap companion to `gc_scan_default_ssl_context_root`.
pub fn gc_update_default_ssl_context_ref(map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let mut slot = default_ssl_context_slot().lock();
    if let Some(ctx_obj) = slot.as_mut() {
        let old = ctx_obj.as_ptr() as usize;
        if let Some(&new) = map.get(&old) {
            debug_assert!(new != 0, "GC pointer map contains null address");
            // SAFETY: `new` is a live, 8-byte-aligned heap address produced
            // by the moving collector for the object previously at `old`.
            *ctx_obj = unsafe { ObjectRef::from_raw(new as *mut u8) };
        }
    }
}

pub fn register_sslengine_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_engine_impl_natives(r);
    register_apply_parameters(r);
    register_ssl_session_real(r);
    r.set_category(__prev_cat);
}

/// Register `javax/net/ssl/SSLSession` accessor natives for the REAL-mode
/// session objects the VM hands out — the 8-field session from
/// `SSLEngineImpl.getSession()` (see ~line 3263) and the 4-field session from
/// `SSLServerSocket.accept()` (see ~line 1014). E42: these two numbers were
/// "7" and "3", the identical off-by-one `session_cipher_slot`'s comment
/// records having carried; the widths are and always were 8 and 4. Both
/// objects carry the bare
/// interface `javax/net/ssl/SSLSession` as their runtime class, so a virtual
/// call resolves to the abstract interface declaration (no Code) and the
/// interpreter's no-Code rescue then looks for a native registered on that
/// class name.
///
/// The *full* SSLSession accessor set lives only in the synthetic-jdk-gated
/// `phases_late::register_p68_ssl` / `tls::register_ssl_session`, which are
/// compiled OUT of the real-JDK CLI. Without a real-mode registration here,
/// Tomcat's `SecureNioChannel`/`SecureNio2Channel` buffer sizing
/// (`SSLEngine.getSession().getApplicationBufferSize()` /
/// `getPacketBufferSize()`) throws
/// `AbstractMethodError: javax/net/ssl/SSLSession.getApplicationBufferSize()I
/// has no Code attribute`, killing the NioEndpoint socket processor so the
/// HTTPS server never serves (~18 TLS/HTTP2-TLS/WebSocket-SSL test classes).
/// Side-table associating an `SSLSession` object with its peer (client)
/// certificate chain (DER, leaf first), populated by the engine's getSession().
/// The real TLS session id (ServerHello `legacy_session_id`) for a session
/// object, keyed by `gc_stable_objref_key` like the other session side-tables.
/// Populated for TLS 1.2 sessions only — see [`peek_server_hello_session_id`].
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — two sites, both of which now
/// compute their `gc_stable_objref_key` before taking the guard (see there).
fn session_wire_id_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, Vec<u8>>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, Vec<u8>>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

fn session_peer_certs_table() -> &'static Mutex<HashMap<u64, Vec<Vec<u8>>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<Vec<u8>>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Side-table associating an `SSLSession` object with ITS OWN (local) certificate
/// chain (DER, leaf first) — the mirror-image of `session_peer_certs_table`,
/// populated by `build_synthetic_ssl_session`. Backs `SSLSession.getLocalCertificates()`
/// (`phases_late::register_p68_ssl`'s registration, which cannot reach a
/// `t27_tls`-private table directly — see `local_certs_for_session`/
/// `record_local_cert_chain` below for the crate-visible accessors).
fn session_local_certs_table() -> &'static Mutex<HashMap<u64, Vec<Vec<u8>>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<Vec<u8>>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Crate-visible accessor for `session_local_certs_table`, used by
/// `phases_late::register_p68_ssl`'s `getLocalCertificates` registration.
pub(crate) fn local_certs_for_session(ctx: &dyn NativeContext, session: ObjectRef) -> Vec<Vec<u8>> {
    session_local_certs_table()
        .lock()
        .get(&gc_stable_objref_key(ctx, session))
        .cloned()
        .unwrap_or_default()
}

/// The crate-visible WRITER for `session_local_certs_table`, named by
/// `session_local_certs_table`'s own doc comment since that table was written —
/// and, until G51, not present in the tree at all. `grep -rn
/// 'record_local_cert_chain'` over `native-builtins/src/` returned exactly one
/// hit, the doc comment promising it. The table had ONE writer, an open-coded
/// `.lock().insert(..)` at the tail of `build_synthetic_ssl_session`, and that
/// function is reached only from the engine path.
///
/// MEASURED consequence, `RSslLiveSession` on `9ae371468` (`target-rel3`),
/// 2026-08-17 — the server side of a completed loopback handshake, through
/// `SSLServerSocket.accept()`, which never goes near
/// `build_synthetic_ssl_session`:
///
/// ```text
/// CK RSslLiveSession server.localPrincipal          = null  WANT CN=localhost
/// CK RSslLiveSession server.localPrincipal.class    = null  WANT javax.security.auth.x500.X500Principal
/// CK RSslLiveSession server.localCertificates.length = -1   WANT 1
/// ```
///
/// All three are one fact: `ssl_security`'s `getLocalCertificates` and
/// `getLocalPrincipal` both read this table (the second derives the subject
/// from the leaf of what the first returns, which is exactly HotSpot's
/// contract), and for an accepted server session it was empty.
///
/// A no-op on an empty chain, for the same reason [`record_client_peer_chain`]
/// is: a client with no configured identity legitimately has none, and
/// `getLocalCertificates()` answering `null` there is the measured HotSpot
/// answer (`client.localCertificates = null`, green today and still green).
pub(crate) fn record_local_cert_chain(
    ctx: &dyn NativeContext,
    session: ObjectRef,
    chain_der: Vec<Vec<u8>>,
) {
    if chain_der.is_empty() {
        return;
    }
    session_local_certs_table()
        .lock()
        .insert(gc_stable_objref_key(ctx, session), chain_der);
}

/// Side-table associating an `SSLSession` object with the ENDPOINT its peer was
/// reached at — `(host, port)` — for the session shapes that have no slot to
/// carry one.
///
/// **Why a side table and not a wider session.** G44-1 §4 enumerated the
/// widening against every width-keyed reader in this file and it fails at every
/// width: at 6 `session_proto_slot`/`session_cipher_slot` SWAP, `sslsess_attrs_slot`
/// goes `None` so the whole attribute API silently no-ops, and
/// `session_has_negotiated` falls into `_ => true` — which makes the NULL
/// session valid again and takes the green `RSslNullSession` with it; at 7 the
/// swap remains and the three green `*.sessionContext.isNull` rows go red.
/// `phases_late::ssl_security::NEW13_SSL_SESS_FIELDS` now carries that table as
/// a "DO NOT WIDEN" block with a test asserting the slot map against this
/// file's two slot functions. This is the shape that costs nothing:
/// [`session_peer_certs_table`] already solves the identical problem — a fact
/// about the peer that the narrow shape has no slot for — keyed the same way,
/// on the session OBJECT.
///
/// **The port is `i32`, not `u16`.** `-1` is HotSpot's measured answer for "no
/// peer", and a table that could only hold `0..=65535` would have to spell that
/// as an absent row, which is a different statement: absent means "nobody
/// recorded an endpoint for this session", and the readers below fall THROUGH
/// an absent row to the socket registry. A recorded `-1` would be a claim.
/// Nothing writes one today; the type is what keeps the distinction available.
///
/// MEASURED, HotSpot 25.0.3+9-LTS `Microsoft-13877124`, this host, 2026-08-17
/// (`scratchpad/g51/G51Probe.java` and `G51Engine.java`, the full
/// peer-endpoint family):
///
/// ```text
///                                              getPeerHost()   getPeerPort()
///   never-connected SSLSocket                  null            -1
///   client SSLSocket dialled by hostname       localhost       the server port
///   client SSLSocket dialled by IP literal     127.0.0.1       the server port
///     ... and the SAME with an explicit SNIHostName("localhost") set:
///         SNI does NOT reach getPeerHost, in either direction
///   the SERVER's view of that handshake        127.0.0.1       the CLIENT's
///                                              (the literal,     ephemeral port
///                                               never reverse    (positive, and
///                                               resolved)        NOT the listener's)
///   SSLEngine, no peer named                   null            -1
///   SSLEngine("example.test", 8443), pre-hs    null            -1
///   SSLEngine("example.test", 8443), post-hs   example.test    8443
///   SSLEngine server side of that handshake    null            -1
///   any of the above after invalidate()        unchanged
/// ```
///
/// Two of those rows are the whole design. **The host is the one the CALLER
/// NAMED, not one derived from the peer's certificate and not one derived from
/// SNI** — the IP-literal row proves it (leaf subject `CN=localhost`, SNI
/// `localhost`, answer `127.0.0.1`). And **the server's peer is the CLIENT**,
/// so its port is an ephemeral one and comparing it to the listener's port is
/// the wrong test; `RSslLiveSession` asserts `> 0` there and `== port` on the
/// client side, and those are different questions on purpose.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both callers evaluate
/// `gc_stable_objref_key` — which calls `ctx.identity_hash_code` — into a local
/// BEFORE acquiring. It used to sit inside the lock expression, which is the
/// same shape the 2026-08-17 round hoisted out of five other tables.
fn session_peer_endpoint_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, (String, i32)>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, (String, i32)>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Record the endpoint a session's peer was reached at. See
/// [`session_peer_endpoint_table`] for the measured contract and for why this
/// is a side table rather than two more fields on the session.
///
/// A no-op when there is nothing to say — an empty host AND a non-positive
/// port. That is not tidiness: the readers below FALL THROUGH an absent row to
/// `session_stream_id`, and a row of `("", -1)` would shadow a real answer the
/// socket registry could still have given.
pub(crate) fn record_session_peer_endpoint(
    ctx: &dyn NativeContext,
    session: ObjectRef,
    host: &str,
    port: i32,
) {
    if host.is_empty() && port <= 0 {
        return;
    }
    let key = gc_stable_objref_key(ctx, session);
    session_peer_endpoint_table()
        .lock()
        .insert(key, (host.to_string(), port));
}

/// The recorded endpoint for a session object, or `None` if none was recorded.
fn session_peer_endpoint(ctx: &dyn NativeContext, session: ObjectRef) -> Option<(String, i32)> {
    let key = gc_stable_objref_key(ctx, session);
    session_peer_endpoint_table().lock().get(&key).cloned()
}

/// The remote address of an accepted rustls server stream, as
/// `(host-literal, port)`.
///
/// `rustls_server_accept_within` binds the accepted peer address as `_peer` and
/// drops it, and `TlsServerStreamEntry` — unlike its client twin, which carries
/// `peer_host`/`peer_port` — has no field for it. Rather than widen the entry
/// for one accessor, this asks the duplicate socket handle the entry already
/// keeps for exactly this class of out-of-band question (`raw`; see
/// [`TlsClientStreamEntry::raw`]).
///
/// The literal is normalised out of IPv4-mapped IPv6 form: a dual-stack
/// listener reports a loopback client as `::ffff:127.0.0.1`, and HotSpot's
/// measured answer is `127.0.0.1` (`G51Probe`, `server.peerHost`, which agrees
/// with the accepted socket's own `getInetAddress().getHostAddress()`).
/// HotSpot does NOT reverse-resolve it to a name, so neither does this.
pub(crate) fn rustls_server_peer_endpoint(rid: i32) -> Option<(String, i32)> {
    let reg = sreg().lock();
    let address = reg
        .server_streams
        .get(&rid)?
        .raw
        .as_ref()?
        .peer_addr()
        .ok()?;
    Some(socket_addr_endpoint(address))
}

/// `(host-literal, port)` for a remote socket address, in the spelling HotSpot
/// answers `SSLSession.getPeerHost()` with.
///
/// Split out of [`rustls_server_peer_endpoint`] because the only interesting
/// thing in it — the IPv4-mapped normalisation — needs no socket to test, and a
/// rule that lives inside a function requiring a live TLS peer is a rule
/// nothing checks. A dual-stack listener reports a loopback client as
/// `::ffff:127.0.0.1`; HotSpot's measured answer is `127.0.0.1`, matching the
/// accepted socket's own `getInetAddress().getHostAddress()`.
fn socket_addr_endpoint(address: std::net::SocketAddr) -> (String, i32) {
    let ip = match address.ip() {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => std::net::IpAddr::V4(v4),
            None => std::net::IpAddr::V6(v6),
        },
        other => other,
    };
    (ip.to_string(), address.port() as i32)
}

/// FIX (netty-https-client-trust residual): populate `session_peer_certs_table`
/// for a CLIENT-side `SSLSession` (allocated by `phases_late::new13_alloc_ssl_session`
/// for the native-tls `SSLSocketFactory.createSocket` path). Without this, the
/// client's own session object never gets an entry — the table was only ever
/// populated for SSLEngine-based (server / NIO) sessions — so any caller that
/// later queries `session.getPeerCertificates()` on the CLIENT session (e.g.
/// Spring's `DefaultSslInfo.initCertificates` when building `SslInfo` for a
/// reactive HTTPS exchange) always sees an empty chain and gets
/// `SSLPeerUnverifiedException("peer not authenticated")` even though the
/// handshake succeeded and a real peer chain was captured (and already used
/// once, to pass the TrustManager check in `new13_do_create_socket`). A no-op
/// when the chain is empty (nothing to record; the accessor's existing
/// empty-chain contract is unaffected).
pub(crate) fn record_client_peer_chain(
    ctx: &dyn NativeContext,
    session: ObjectRef,
    chain_der: Vec<Vec<u8>>,
) {
    if chain_der.is_empty() {
        return;
    }
    session_peer_certs_table()
        .lock()
        .insert(gc_stable_objref_key(ctx, session), chain_der);
}

/// The TLS STREAM ID recorded in a session's slot 2, for the widths on which
/// slot 2 *is* a stream id — and `None` on every other width.
///
/// F18. This exists because two accessors were reading slot 2 as a `servlet`
/// registry key on any shape with three or more fields, and slot 2 does not
/// mean the same thing at every width. The table is
/// [`session_has_negotiated`]'s, restated as the question these callers
/// actually ask:
///
/// | width | minted by | slot 2 | is it a stream id? |
/// |---|---|---|---|
/// | 3 | (retired — E42) | stream id | n/a, nothing mints it |
/// | 4 | `ssl_security`'s NEW-13 shape; `SSLServerSocket.accept` | stream id (or `-1`, or `HTTPS_CLIENT_SESSION_MARKER`) | **yes** |
/// | 6 | `tls.rs::init_ssl_session_fields`; `http2.rs` | `tls.rs::SES_VALID` | **no** |
/// | 8 | `build_synthetic_ssl_session` | the `isValid` flag | **no** |
///
/// **The bug this closes, which is a cross-connection one.**
/// `ssl_security::getPeerPrincipal` gated on `> NEW13_SESS_TLSID`, i.e. width
/// >= 3, so on an 8-field engine session it read the *`isValid` flag* — `0` or
/// `1` — and looked that up in `servlet::s2_tls_peer_cert_chain_der`. Ids `0`
/// and `1` are not unreachable: `s2_next_free_id` starts its counter at `1`
/// (`servlet.rs`), so id `1` is the FIRST id the socket registry hands out.
/// An engine session that happened to be valid could therefore be handed the
/// peer certificate chain of an unrelated socket. E31-1 NOMINATION 4, which
/// re-raised E22-1 NOMINATION B.
///
/// Ids below zero are rejected here rather than at the call sites: `-1` is the
/// "never connected" sentinel every minter writes, and a negative registry key
/// is meaningless in every table that consumes one.
pub(crate) fn session_stream_id(ctx: &dyn NativeContext, session: ObjectRef) -> Option<i32> {
    match ctx.object_num_fields(session) {
        3 | 4 => match ctx.get_field(session, 2) {
            Value::Int(id) if id >= 0 => Some(id),
            _ => None,
        },
        _ => None,
    }
}

/// **The one resolver for "what certificate chain did this session's peer
/// present".** Every door that answers a question about the peer's identity
/// must come through here.
///
/// F18. `SSLSession.getPeerCertificates` (this file) and
/// `SSLSession.getPeerPrincipal` (`phases_late::ssl_security`) are registered
/// by different registrars and, until now, read *different sources* for the
/// same fact:
///
/// * `getPeerCertificates` read [`session_peer_certs_table`], keyed on the
///   session OBJECT;
/// * `getPeerPrincipal` read `servlet::s2_tls_peer_cert_chain_der(slot2)`,
///   keyed on the TLS STREAM ID.
///
/// MEASURED on HotSpot 25.0.3+9-LTS (this host,
/// `scratchpad/f18/F18SessionContract.java`, loopback `HttpsServer` +
/// `HttpsURLConnection`, three runs byte-identical): on a completed handshake
/// the two agree, and `getPeerPrincipal()` is exactly the leaf certificate's
/// subject —
/// `getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal())` is
/// `true`. On CratonVM they disagreed: an HTTPS client session is registered
/// in the object table by `record_client_peer_chain` and has NO entry in the
/// socket registry, so `getPeerCertificates()` returned the chain while
/// `getPeerPrincipal()` on the SAME OBJECT, in the same call sequence, threw
/// `SSLPeerUnverifiedException`. F10-1 NOMINATION 2.
///
/// **Order is load-bearing, and it shrinks a collision surface rather than
/// widening one.** The object table is consulted FIRST. That table is the one
/// populated for HTTPS client sessions, whose slot 2 carries
/// `net_phase_e::HTTPS_CLIENT_SESSION_MARKER` — a value chosen to MISS the
/// socket registry. Trying the object table first means the common HTTPS case
/// never reaches the id lookup at all, so the marker's miss is now a
/// second-line guarantee instead of the only one. It still has to hold, and it
/// does: the marker is `0x0800_0000`, strictly below
/// `servlet::PENDING_CONNECT_SOCK_ID_BASE` (`0x1000_0000`),
/// `PENDING_LAYERED_SOCK_ID_BASE` (`0x2000_0000`) and `RUSTLS_SOCK_ID_BASE`
/// (`0x4000_0000`), and far above the monotonic `s2_next_free_id` counter that
/// starts at `1` — re-verified against `servlet.rs` for F18, not taken on
/// trust. Being below `RUSTLS_SOCK_ID_BASE` also keeps it off
/// `s2_tls_peer_cert_chain_der`'s rustls redirect.
///
/// **Not implemented by calling `getPeerCertificates` through the interpreter.**
/// That was F10-1 NOMINATION 2's suggested shape and it is the more expensive
/// one: it would build `X509CertImpl` mirrors only for `getPeerPrincipal` to
/// pull a subject string back out of them. Sharing the DER resolver gives the
/// same single-source-of-truth property — the two doors cannot see different
/// chains, because there is only one function that decides — without the round
/// trip.
pub(crate) fn peer_certs_for_session(ctx: &dyn NativeContext, session: ObjectRef) -> Vec<Vec<u8>> {
    if let Some(chain) = session_peer_certs_table()
        .lock()
        .get(&gc_stable_objref_key(ctx, session))
        .cloned()
    {
        if !chain.is_empty() {
            return chain;
        }
    }
    if let Some(id) = session_stream_id(ctx, session) {
        if let Some(chain) = servlet::s2_tls_peer_cert_chain_der(id) {
            return chain;
        }
    }
    Vec::new()
}

/// The set of `javax/net/ssl/SSLSession` objects on which `invalidate()` has
/// been called, keyed the same GC-stable way as
/// [`session_peer_certs_table`] and `session_local_certs_table`.
///
/// F18 — **why a side table and not a field.** `SSLSession.invalidate()` had
/// no real-JDK-mode registration at all (`grep` over the whole crate returned
/// exactly one, in `tls.rs`, whose registrar is `#[cfg(feature =
/// "synthetic-jdk")]`), so in the mode `--jdk-only` runs the interface
/// declaration with no Code attribute was what ran: an `AbstractMethodError`.
/// F10-1 NOMINATION 3 split the fix in two and called the second half blocked,
/// because recording an `invalidated` bit "needs a slot this shape does not
/// have" and both F6-1 §4 and E42-1 §2 reject widening the shape again.
///
/// That premise is true and the conclusion does not follow: this file already
/// carries two per-session facts that no slot holds, in exactly this form. The
/// bit does not need to live in the object.
///
/// **Why widening would have been the worse fix, stated rather than implied.**
/// The width of a session shape is what every reader in this file uses to
/// decide what its slots MEAN (`session_cipher_slot`, `session_proto_slot`,
/// `sslsess_attrs_slot`, [`session_stream_id`], `session_has_negotiated`).
/// Widening the NEW-13 shape from 4 to 5 would move it across the `n >= 6`
/// boundary's near side and re-open the co-requisite E42-1 records as BLOCKING.
/// A side table changes no width and therefore no reader.
///
/// **This table is inert until an application calls `invalidate()`.** It is
/// empty in every process that never does, so [`session_is_valid`] answers
/// exactly what `session_has_negotiated` alone answered before F18 — which is
/// the property that makes this change safe to land without a TLS fixture.
///
/// **Growth is bounded by `invalidate()` calls, not by session mints.** F10-1
/// §8.5 records that `session_peer_certs_table` grows once per minted session
/// and is never pruned, which on the HTTPS path is once per accessor call.
/// This table is only written by the one door, so it does not inherit that
/// rate. It does inherit the 32-bit key width: `gc_stable_objref_key` is
/// `identity_hash_code(o) as u32`, so a birthday collision here would report a
/// *live* session as invalidated. That is the same exposure the two cert
/// tables already carry and it is not made worse by a table that is empty in
/// the common case; the key width is a separate fix for all three.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Same hoist as
/// [`session_peer_endpoint_table`]: `gc_stable_objref_key` is evaluated into a
/// local before either acquisition, so no `NativeContext` call runs under the
/// guard.
fn session_invalidated_table(
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

/// Record that `invalidate()` was called on this session. Crate-visible so
/// `tls.rs`'s `--synthetic-jdk` copy writes the same state this file's
/// real-mode copy reads — one bit, one owner, both modes.
///
/// **Deliberately a no-op for a session that negotiated nothing**, and that is
/// HotSpot's behaviour rather than an optimisation. MEASURED, HotSpot
/// 25.0.3+9-LTS (`scratchpad/f18/F18SessionContract.java`, three runs
/// byte-identical), on the unconnected `SSLSocket`'s session and on the
/// pre-handshake `SSLEngine`'s alike: every accessor reads identically before
/// and after `invalidate()`, because `isValid()` was already `false` and there
/// was nothing else to invalidate. Since [`session_is_valid`] is
/// `negotiated && !invalidated`, recording the bit for a never-negotiated
/// session could not change an answer either — so the gate costs no fidelity
/// and keeps the table from growing on the one door that mints sessions
/// nobody handshaked.
pub(crate) fn session_mark_invalidated(ctx: &dyn NativeContext, session: ObjectRef) {
    if !session_has_negotiated(ctx, session) {
        return;
    }
    let key = gc_stable_objref_key(ctx, session);
    session_invalidated_table().lock().insert(key);
}

/// **The one predicate behind `SSLSession.isValid()`, in both modes.**
///
/// F18. HotSpot's `isValid()` is `SSLSessionImpl.isRejoinable()`, which is
/// `sessionId.length() != 0 && !invalidated && ...`
/// (`sun/security/ssl/SSLSessionImpl.java:788`) — TWO pieces of state. Before
/// F18 this VM modelled only the first, because nothing in the shipping mode
/// could write the second.
///
/// MEASURED, HotSpot 25.0.3+9-LTS, on a session that GENUINELY negotiated (a
/// completed loopback `HttpsURLConnection` handshake — the population F10 made
/// reachable, and the first time this contract has been measured on one rather
/// than on a null session), `scratchpad/f18/F18SessionContract.java`, three
/// runs byte-identical:
///
/// ```text
///   before invalidate()  isValid=true   id=byte[32]  cipher=TLS_AES_256_GCM_SHA384
///                        protocol=TLSv1.3  peerPrincipal=CN=localhost,...
///                        peerCerts=1  sessionContext=SSLSessionContextImpl
///   after  invalidate()  isValid=FALSE  id=byte[32]  cipher=TLS_AES_256_GCM_SHA384
///                        protocol=TLSv1.3  peerPrincipal=CN=localhost,...
///                        peerCerts=1  sessionContext=NULL
///   invalidate() twice   isValid=false            (idempotent)
///   a second, untouched connection's session      isValid=true, unaffected
/// ```
///
/// So `invalidate()` moves `isValid()` — and, a detail F10 did not report and
/// this lane measured, `getSessionContext()`, which drops to `null`. It moves
/// NOTHING else: the id keeps its 32 bytes and its exact contents, and the
/// cipher, protocol, peer principal and peer chain all survive. That is why
/// `getId()` is deliberately gated on `session_has_negotiated` ALONE and not
/// on this function — tying the id to validity would make `invalidate()` erase
/// the id, a new divergence traded for an old one. `getSessionContext()`'s
/// half of the contract is free here: this VM answers `null` in every state
/// (see its registration), so it is already on the right side of the
/// invalidated case and merely under-reports the live one.
pub(crate) fn session_is_valid(ctx: &dyn NativeContext, session: ObjectRef) -> bool {
    if !session_has_negotiated(ctx, session) {
        return false;
    }
    let key = gc_stable_objref_key(ctx, session);
    !session_invalidated_table().lock().contains(&key)
}

/// Did the `javax/net/ssl/SSLSession` object `this` actually negotiate
/// anything?
///
/// This is the ONE predicate behind `SSLSession.isValid()` and the "does this
/// session have an id at all" test in `getId()`, because HotSpot answers both
/// from the same underlying state — measured, HotSpot 25.0.3+9-LTS
/// (`scratchpad/e12/E12SessionContract.java`, recorded in
/// `docs/known-issues/jdk-only/E12-1-the-null-session-and-the-fabricated-cipher.md`
/// §1):
///
/// ```text
///   nothing negotiated:  isValid() = false   getId() = byte[0]
///   after a handshake:   isValid() = true    getId() = byte[32]
/// ```
///
/// Keeping the two in one function is deliberate. E3-1's recorded lesson in
/// this same family is that a rule spread over N call sites drifts, and this
/// rule now has to hold across THREE different widths of synthetic
/// `SSLSession` — the "nothing negotiated" signal lives in a different place
/// in each, so the field count is the discriminator (the same convention
/// `getProtocol`/`getCipherSuite` already use):
///
/// | width | minted by | slot 2 holds | negotiated when |
/// |---|---|---|---|
/// | 4 | `ssl_security::new13_alloc_{,null_}ssl_session`; `SSLServerSocket.accept` in this file | `NEW13_SESS_TLSID` — the stream id | a stream id was recorded (`>= 0`); the NULL session carries `-1` |
/// | 6 | `http2.rs`'s `HttpResponse.sslSession()`; `tls.rs` | — | always — the shape is only minted after a handshake |
/// | 8 | `build_synthetic_ssl_session` in this file | the `isValid` flag | the flag is set |
///
/// E42 — **widths 2 and 3 were RETIRED, and the 4-row is the merge of what
/// used to be two rows.** `ssl_security`'s NEW-13 shape widened from 3 to 4 so
/// that `putValue` has a slot of its own (see `NEW13_SSL_SESS_FIELDS` and
/// `sslsess_attrs_slot`), and its 2-field `SSLEngine.getSession` fallback now
/// calls `new13_alloc_null_ssl_session` like everything else. Width 4 is
/// byte-identical to the accept shape — proto, cipher, streamId, attrs — so
/// this is ONE row and not a coincidence of two.
///
/// The width-4 row is the one that decides an unconnected `SSLSocket`: a
/// field-count test alone would treat every narrow shape as negotiated and
/// leave the null session answering `isValid() = true` / a 32-byte id, which
/// is the exact defect in the mode this file is live in. The `0..=3` arms are
/// kept as defensive lower bounds — nothing mints those widths any more, and
/// nothing may be allowed to answer "negotiated" if something starts.
///
/// E31: `pub(crate)` since 2026-08-13. `tls.rs` had grown its OWN width-blind
/// copies of this question (`isValid` on `javax/net/ssl/SSLSession` and again
/// on `sun/security/ssl/SSLSessionImpl`, each testing a field count and
/// answering `true` for everything narrower), which is how the null session
/// stayed valid under `--synthetic-jdk` after this predicate fixed it in the
/// default mode. There is one rule; it lives here; both files call it.
pub(crate) fn session_has_negotiated(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    match ctx.object_num_fields(this) {
        // No slot at all, and never minted for a negotiated session.
        0..=2 => false,
        // E42: 3 and 4 are ONE arm. `ssl_security`'s NEW-13 shape widened from
        // 3 to 4 so that `putValue` has a slot of its own instead of writing a
        // HashMap over the stream id (see `NEW13_SSL_SESS_FIELDS`), and width 4
        // is byte-identical to `SSLServerSocket.accept`'s shape below: proto,
        // cipher, streamId, attrs.
        //
        // Merging them is a no-op for the accept shape and the fix for the
        // widened one. accept writes `RUSTLS_SOCK_ID_BASE + stream_id` into
        // slot 2, which is always `>= 0`, so `_ => true`'s answer for it is
        // unchanged — and that arm's premise ("4- and 6-field shapes are only
        // minted after a handshake") is now FALSE for width 4, which is
        // precisely why it cannot stay. `ssl_security`'s null session carries
        // `Int(-1)`.
        //
        // Pinned from the other side by
        // `phases_late::ssl_security::new13_tests
        // ::the_widened_null_session_is_still_not_negotiated`, which calls this
        // function directly: if this arm regresses, that test fails rather than
        // `RSslNullSession` silently reporting a valid null session again. The
        // in-file pair `the_null_socket_session_has_no_id_and_is_not_valid` /
        // `a_session_that_negotiated_keeps_its_id_and_its_validity` covers both
        // directions at both widths without leaving this crate.
        3 | 4 => match ctx.get_field(this, 2) {
            Value::Int(tls_id) => tls_id >= 0,
            // Defensive: a non-`Int` here used to mean `putValue` had
            // overwritten the stream id — the collision the widening removes.
            // Kept because this predicate is also reached from `tls.rs` and
            // from any future minter that has not been audited: an unreadable
            // slot must not silently answer "never negotiated" for a session
            // that did.
            _ => true,
        },
        n if n >= 7 => matches!(ctx.get_field(this, 2), Value::Int(v) if v != 0),
        // The 6-field shape is only minted after a handshake.
        _ => true,
    }
}

/// The slot holding the negotiated CIPHER SUITE, by shape width — and its
/// twin below for the PROTOCOL. Two conventions exist and the width is what
/// separates them:
///
/// | width | minted by | slot 0 | slot 1 |
/// |---|---|---|---|
/// | 2 | `ssl_security`'s `SSLEngine.getSession` fallback | protocol | cipher |
/// | 3 | `ssl_security::new13_alloc_{,null_}ssl_session` | protocol | cipher |
/// | 4 | `SSLServerSocket.accept` in this file | protocol | cipher |
/// | 6 | `tls.rs::init_ssl_session_fields`; `http2.rs` | **cipher** | **protocol** |
/// | 8 | `build_synthetic_ssl_session` in this file | **cipher** | **protocol** |
///
/// E31: the threshold used to be `>= 7`, which put the 6-field row on the
/// wrong side of the line — `getProtocol()` on a `tls.rs` session read its
/// cipher slot and vice versa. That was inert only because the 6-field shape
/// stores `Int` indices rather than `String`s, so both accessors fell through
/// to the sentinel arm instead of returning the *other* value; a producer that
/// ever wrote real names into that shape would have swapped them. Stating the
/// rule as `>= 6` makes it true rather than harmlessly false, and changes no
/// answer today (verified against every minter in the table).
///
/// `None` for a shape too short to carry the pair at all. Both accessors used
/// to index slot 0 or 1 with no floor, so a foreign or 1-field receiver was an
/// out-of-range field read on a class the real JDK also defines — the shape
/// this directory records as a real bug rather than a miss. Returning an
/// `Option` is what makes that check unskippable at the call site.
pub(crate) fn session_cipher_slot(num_fields: usize) -> Option<usize> {
    match num_fields {
        0..=1 => None,
        n if n >= 6 => Some(0),
        _ => Some(1),
    }
}

/// See [`session_cipher_slot`] for the width table this mirrors.
pub(crate) fn session_proto_slot(num_fields: usize) -> Option<usize> {
    match num_fields {
        0..=1 => None,
        n if n >= 6 => Some(1),
        _ => Some(0),
    }
}

/// The dedicated `SSLSession` attribute-map slot for this shape, or `None` for
/// a shape that has no slot to spare.
///
/// **E31 — this replaces a bare `num_fields - 1` at five call sites, and the
/// bare form was a live slot collision.** `num_fields - 1` is the attribute
/// slot on some widths and a field that means something else on others, and
/// `putValue` wrote a `java.util.HashMap` straight over it:
///
/// | width | `num_fields - 1` is | what a `putValue` destroyed |
/// |---|---|---|
/// | 4 | a dedicated attrs slot | nothing |
/// | 6 | `tls.rs::SES_CREATION_TIME` | `getCreationTime()` returns an object reference through a `()J` descriptor |
/// | 8 | a dedicated attrs slot | nothing |
///
/// E42 — **the 2- and 3-field rows are gone, and they were the two worst.**
/// The 2-field `SSLEngine.getSession` fallback now calls
/// `ssl_security::new13_alloc_null_ssl_session`, and the 3-field NEW-13 shape
/// widened to 4 *for this reason*: on it, `num_fields - 1` was
/// `ssl_security::NEW13_SESS_TLSID`, which is also the slot
/// [`session_has_negotiated`] reads to decide whether anything was negotiated.
/// A single `putValue` on an unconnected socket's session therefore turned
/// `Int(-1)` ("nothing negotiated") into `Object(Some(map))`, which took that
/// function's defensive `_ => true` arm — so `isValid()` went back to `true`
/// and `getId()` went back to 32 fabricated bytes. **A `putValue` resurrected
/// the exact fabrication the E12/E22 lanes removed**, on the one shape whose
/// whole purpose is to represent "no handshake happened". The defensive arm is
/// what kept that from being a *new* wrong answer relative to the pre-E12
/// code; it was never a reason the collision was safe.
///
/// Note the two halves of the width-4 agreement, because either alone is a
/// silent regression: `4 => Some(3)` is only correct while
/// `ssl_security::NEW13_SESS_ATTRS == NEW13_SSL_SESS_FIELDS - 1` and
/// `!= NEW13_SESS_TLSID`, and slot 2 is only a stream id while
/// [`session_has_negotiated`]'s `3 | 4` arm reads it as one. Both are held by
/// tests rather than by this comment — this file's
/// `tests::a_widened_null_session_put_value_does_not_touch_the_stream_id`, and
/// `phases_late::ssl_security::new13_tests
/// ::the_attribute_slot_is_the_last_one_and_is_not_the_stream_id` for the
/// constants' side of the agreement.
///
/// Jetty's `SecureRequestCustomizer.retrieveSni()` calls `getValue()` then
/// `putValue()` on every SSL request, so the writer is not hypothetical — it
/// is the caller the attribute API was registered for in the first place.
///
/// Returning `None` (rather than widening the shapes) was the change that
/// could not break anything else: `putValue`/`removeValue` become no-ops and
/// `getValue`/`getValueNames` answer null/`String[0]`, which is what those
/// shapes already answer today for a session nobody has written to. The
/// divergence that remains — a `putValue` on a 6-field session does not
/// round-trip through `getValue` — is a *quiet* miss on a JSSE convenience
/// API, against a *loud* corruption of the negotiation state.
///
/// E42 closed the 3-field half of that residual by widening rather than by
/// refusing, together with the `session_has_negotiated` arm merge it required;
/// see `docs/known-issues/jdk-only/E42-1-the-slot-that-was-never-there-and-the-predicate-that-was-its-own-negation.md`.
/// The 6-field shape is now the **only** one with no attribute slot, and it is
/// what `_ => None` is for.
fn sslsess_attrs_slot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    match ctx.object_num_fields(this) {
        4 => Some(3),
        n if n >= 7 => Some(n - 1),
        _ => None,
    }
}

/// Fire `SSLSessionBindingListener.valueBound`/`valueUnbound` for a value that
/// implements the interface, the way `SSLSessionImpl.putValue`/`removeValue`
/// do.
///
/// JSSE's contract is explicit: "if the object implements
/// SSLSessionBindingListener, the valueBound method is called". netty's
/// `SSLEngineTest.assertSSLSessionBindingEventValue` is a listener that
/// records the event it was handed and asserts on `event.getName()`; with no
/// callback the recorded event stayed null and the test died on
/// `NullPointerException: Cannot invoke
/// "javax.net.ssl.SSLSessionBindingEvent.getName()" because "event" is null`.
///
/// A value that is not a listener, or an event that cannot be constructed, is
/// silently skipped — the attribute store is the primary effect and must not
/// fail because of a callback.
fn fire_session_binding(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
    name: Value,
    value: Value,
    bound: bool,
) {
    let Value::Object(Some(v)) = value else {
        return;
    };
    let Some(iface) = ctx.class_id_by_name("javax/net/ssl/SSLSessionBindingListener") else {
        return;
    };
    if !ctx.is_subclass(ctx.class_id_of_object(v), iface) {
        return;
    }
    let ses_pin = ctx.pin_native_root(session);
    let v_pin = ctx.pin_native_root(v);
    let ses_now = ctx.read_native_pin(ses_pin, session);
    let event = ctx.new_object_initialized(
        "javax/net/ssl/SSLSessionBindingEvent",
        "(Ljavax/net/ssl/SSLSession;Ljava/lang/String;)V",
        &[Value::Object(Some(ses_now)), name],
    );
    if let Ok(Some(Value::Object(Some(ev)))) = event {
        let ev_pin = ctx.pin_native_root(ev);
        let v_now = ctx.read_native_pin(v_pin, v);
        let ev_now = ctx.read_native_pin(ev_pin, ev);
        let method = if bound { "valueBound" } else { "valueUnbound" };
        let _ = ctx.invoke_virtual(
            v_now,
            method,
            "(Ljavax/net/ssl/SSLSessionBindingEvent;)V",
            &[Value::Object(Some(ev_now))],
        );
    }
    ctx.unpin_native_roots(ses_pin);
}

fn register_ssl_session_real(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLSession";

    // toString() — JSSE's `SSLSessionImpl.toString()` is
    // `"Session(" + creationTime + "|" + getCipherSuite() + ")"`, and it is the
    // tail of `SSLEngineImpl.toString()` / `SSLSocketImpl.toString()` as well as
    // an answer in its own right. CratonVM's session objects carry the bare
    // `javax/net/ssl/SSLSession` INTERFACE as their class, so with no
    // registration here a virtual `toString()` resolves to `Object.toString()`
    // and prints an identity hash — which is not wrong so much as useless, and
    // it is the half of `SSLEngine.toString()` a reader actually wants.
    //
    // Both fields come from this file's own accessors, so a session that cannot
    // answer one still renders rather than throwing: JSSE's own creationTime is
    // a `long` and its cipher suite is never null (it is
    // `SSL_NULL_WITH_NULL_NULL` before a handshake), so 0 and the JSSE null
    // suite are the values that make the string mean what it means.
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let created = match ctx.invoke_virtual(this, "getCreationTime", "()J", &[]) {
            Ok(Some(v)) => v.as_long().unwrap_or(0),
            _ => 0,
        };
        let suite = match ctx.invoke_virtual(this, "getCipherSuite", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx
                .read_string(s)
                .unwrap_or_else(|| crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.into()),
            _ => crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.to_string(),
        };
        let text = format!("Session({created}|{suite})");
        let s = ctx.create_string(&text);
        Ok(Some(Value::Object(Some(s))))
    });

    // getPeerCertificates() — the client certificate chain, for mTLS. Tomcat's
    // SSLAuthenticator / coyote SSLSupport reads this to authenticate the
    // client; without it a client-cert-protected resource returns HTTP 401.
    // Build real `sun.security.x509.X509CertImpl` mirrors from the captured DER
    // (same path the keystore uses). Empty chain → throw
    // SSLPeerUnverifiedException (real-JDK contract), which Tomcat treats as
    // "no client cert".
    //
    // F18: the chain now comes from `peer_certs_for_session` rather than from
    // an inline `session_peer_certs_table` lookup, so this door and
    // `ssl_security`'s `getPeerPrincipal` cannot see different chains for one
    // session — they are the same function call. See that helper for the
    // measured HotSpot contract the two have to agree on. This is a strict
    // widening for THIS door: it keeps the object-table answer it already gave
    // and gains the stream-id fallback its neighbour had, which is why the
    // change cannot make an answer that used to be a chain become empty.
    r.register(
        cls,
        "getPeerCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let chain = peer_certs_for_session(ctx, this);
            if chain.is_empty() {
                // Real-JDK contract (and this function's own doc): throw
                // SSLPeerUnverifiedException — an SSLException — NOT
                // IllegalStateException. Callers specifically catch the
                // former to mean "peer presented no certificate": e.g.
                // Spring's DefaultSslInfo.initCertificates() swallows
                // SSLPeerUnverifiedException when building SslInfo for a
                // server session without client auth; the previous
                // IllegalStateException escaped instead and failed every
                // reactive HTTPS request
                // (ServerHttpsRequestIntegrationTests::checkUri).
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/net/ssl/SSLPeerUnverifiedException",
                    "peer not authenticated",
                ));
            }
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), chain.len());
            for (i, der) in chain.iter().enumerate() {
                let mirror = crate::keystore::make_x509_mirror(ctx, "peer", der)?;
                ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // KEEP THE VALUES — but the claim that used to head this block was WRONG,
    // and wrong in the confident register that stops the next reader checking.
    // It said "KEEP (correct constants…) the real JDK returns 16384 for
    // getApplicationBufferSize". Measured on HotSpot 25.0.3+9-LTS
    // (`scratchpad/e12/E12SessionContract.java`, E12-1 §1):
    //
    //     getApplicationBufferSize()  null session      = 16704
    //     getApplicationBufferSize()  after TLS 1.3     = 16676
    //     getPacketBufferSize()       every state       = 16709
    //
    // So `getPacketBufferSize` is right and `getApplicationBufferSize` is NOT
    // a constant in real JSSE at all: `SSLSessionImpl` derives it from the
    // packet size minus the negotiated suite's record expansion, which is why
    // it moves when a suite is negotiated. 16384 is the RFC 8446 §5.1
    // TLSPlaintext cap, i.e. a floor rather than the JDK's answer.
    //
    // G25 — the number is fixed now too, and the audit that comment asked for
    // was done. RE-MEASURED (`G25Probe`, HotSpot 25.0.3+9), one row per state,
    // both suites, over a real in-memory TLS 1.3 handshake:
    //
    //     never negotiated                     app = 16704   packet = 16709
    //     TLS 1.3 / TLS_AES_256_GCM_SHA384     app = 16676   packet = 16709
    //     TLS 1.3 / TLS_AES_128_GCM_SHA256     app = 16676   packet = 16709
    //
    // Two states, two answers, and no third: within TLS 1.3 the value does not
    // move with the suite, so a `negotiated?` predicate is the whole of the
    // state-dependence and no per-suite table is needed. 16384 was neither
    // value — it is the RFC 8446 §5.1 TLSPlaintext cap, a floor.
    //
    // **Why raising it is safe, which is the part the old comment could not
    // check.** `do_wrap` reads at most 16384 bytes of plaintext out of `srcs`
    // per call (`bb_read_into(ctx, *bb, &mut app_bytes, 16384)` and the
    // `app_bytes.len() >= 16384` break) whatever the caller offers, and then
    // drains only COMPLETE records that fit the destination. So a caller that
    // sizes its application buffer from this accessor and fills it hands over
    // 16704 bytes, this engine consumes 16384 of them, emits one record of at
    // most 16406 into a `getPacketBufferSize()`-sized destination, and reports
    // the true `bytesConsumed`. There is no state in which the larger number
    // makes a record that cannot fit — the livelock the old comment feared
    // needs the engine to promise a record bigger than its packet buffer, and
    // the 16384 cap is what stops it. On `unwrap`, a larger destination is
    // only ever safer.
    //
    // The under-report was not free: a caller that sizes a receive buffer from
    // this accessor and a peer that fills a genuine 16676-byte application
    // record put 292 bytes more on the wire than the buffer holds, which is a
    // `BUFFER_OVERFLOW` retry loop against a buffer the caller has already
    // been told is big enough.
    //
    // Same correction is owed to the twin in `tls.rs` — see E22-1's
    // NOMINATION, and note that the twin is `#[cfg(feature =
    // "synthetic-jdk")]`-only, so THIS copy is the live one by default.
    //
    // SHADOWING (wave 4 correction — the wave-3 note was wrong): `tls.rs
    // ::register_ssl_session` registers the same two triples, but its registrar
    // `register_tls_natives` is reached ONLY from `register_synthetic_overrides`,
    // which is `#[cfg(feature = "synthetic-jdk")]`. In the
    // DEFAULT real-JDK build THIS copy is the live one and the tls.rs pair does
    // not exist; under `--synthetic-jdk` tls.rs runs later and wins. The values
    // are identical either way — if you change one, change both.
    r.register(cls, "getApplicationBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(if session_has_negotiated(ctx, this) {
            JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED
        } else {
            JSSE_APPLICATION_BUFFER_SIZE_FRESH
        })))
    });
    r.register(cls, "getPacketBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(JSSE_PACKET_BUFFER_SIZE)))
    });
    // getId() — Tomcat's request/auth plumbing reads the TLS session id (e.g.
    // for SSL session tracking / client-cert requests). Real JDK returns the
    // negotiated session id bytes; the abstract interface declaration has no
    // Code, so without a real-mode native this throws AbstractMethodError and
    // every HTTPS request to a protected resource fails (HTTP -1). Return a
    // stable 32-byte id derived from the session object's identity — but ONLY
    // for a session that actually negotiated one.
    //
    // E12: a session that negotiated nothing has NO id, and JSSE says so with
    // `byte[0]` rather than 32 plausible bytes. This has a named, measured
    // consumer: Tomcat's `JSSESupport.getSessionId`
    // (java/org/apache/tomcat/util/net/jsse/JSSESupport.java:171) is
    // `if (ssl_session == null || ssl_session.length == 0) return null;` — it
    // tests `length == 0` EXACTLY, so the fabricated 32 bytes are precisely
    // the value that defeats it and makes an unhandshaken session present as
    // a trackable one. Measured HotSpot 25.0.3+9-LTS, E12-1 §1/§4.
    r.register(cls, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // ONE gate, and G7 removed the second one. `session_has_negotiated` is
        // the width-aware predicate every other door in this file uses
        // (E12/E42): on the 4-field socket shapes it reads the stream id in
        // slot 2, and a session that negotiated nothing carries `-1`; on the
        // 7/8-field engine shape it reads the `isValid` flag that
        // `build_synthetic_ssl_session` writes from
        // `conn.negotiated_cipher_suite().is_some()`.
        //
        // **The second gate produced `byte[0]` in exactly one state, and that
        // is the state where the oracle says 32 bytes.** It was
        // `engine_shape && !session_is_negotiated(...)` —
        // `negotiated_session_keys` membership, which `engine_session_for`
        // writes only in the handshake-COMPLETED epoch. Enumerate what it could
        // add over the first gate:
        //
        //   * fresh engine, no `conn`      slot 2 = 0 -> gate 1 already refuses
        //   * mid-handshake, past ServerHello  slot 2 = 1, not yet in the set
        //     -> gate 1 admits, gate 2 REFUSED
        //   * completed                    slot 2 = 1, in the set -> both admit
        //
        // So the only row it decided was mid-handshake. MEASURED, HotSpot
        // 25.0.3+9-LTS, `scratchpad/g7/TlsProbe.java`, sampled inside
        // `X509ExtendedTrustManager.checkServerTrusted(chain, authType, Socket)`
        // and again inside the `SSLEngine` overload:
        //
        // ```text
        //   getId          = byte[32] 2310720247a7e8...  (and byte-identical to
        //                    the id the COMPLETED session then reported)
        //   getCipherSuite = "TLS_AES_256_GCM_SHA384"
        //   getProtocol    = "TLSv1.3"
        //   isValid        = true
        //   getSessionContext        = null
        //   getPeerCertificates      = THROWS SSLPeerUnverifiedException
        // ```
        //
        // netty's `SSLEngineTest.testSSLSessionId` — the test the second gate
        // was added for — asserts `assertEquals(0, engine.getSession().getId()
        // .length)` on a FRESHLY CREATED engine, which is the first row above,
        // and gate 1 alone already answers it: no `conn` means no negotiated
        // cipher suite means slot 2 is `0`. Dropping gate 2 cannot regress it.
        //
        // The gate itself was not deleted — it moved to `getSessionContext`,
        // which is the door the oracle shows needs exactly this discrimination
        // (`null` mid-handshake, non-null once complete).
        //
        // RESIDUAL, and it is a real one: HotSpot's handshake session IS the
        // session, so the 32 bytes read mid-handshake are the same 32 bytes the
        // completed session reports. Here `engine_session_for` caches on
        // `(engine, handshaked)`, so the two are DIFFERENT objects and the
        // pseudo-id below — seeded from `gc_stable_objref_key` — differs
        // between them. The LENGTH is now right (which is what Tomcat's
        // `JSSESupport.getSessionId` tests, `length == 0` exactly) and the
        // CONTINUITY is not. Merging the two cache entries is not a free fix:
        // the pre-handshake entry has the "nothing negotiated" sentinels frozen
        // into slots 0/1 at mint time, so reusing that object after the
        // handshake would make a completed session report
        // `SSL_NULL_WITH_NULL_NULL`. Recorded in G7-1 §4 rather than half-done.
        //
        // Hoisted into a `let` rather than chained inline: `session_has_negotiated`
        // takes `&dyn`, and this file already records (see `getPeerHost`) how
        // easily a reborrow in a compound scrutinee collides with an `&mut`
        // beside it.
        let negotiated = session_has_negotiated(ctx, this);
        if !negotiated {
            return Ok(Some(Value::Object(Some(
                ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0),
            ))));
        }
        // The REAL id, when there is one: a TLS 1.2 handshake puts a session id
        // on the wire and both engines saw it, so both answer the same bytes.
        // `SSLEngineTest.testSSLSessionId` asserts exactly that equality — a
        // per-object pseudo-id can never satisfy it. (Under TLS 1.3 nothing is
        // recorded here and the pseudo-id below stands, which is what the same
        // test's `assertFalse(Arrays.equals(...))` branch wants.)
        // Key before the guard — see the store site.
        let wire_key = gc_stable_objref_key(ctx, this);
        let wire_id = session_wire_id_table().lock().get(&wire_key).cloned();
        if let Some(id) = wire_id {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, id.len());
            for (i, b) in id.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
        let seed = gc_stable_objref_key(ctx, this);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        // SplitMix64-style fill so the 32 bytes are stable per session and not
        // all-identical (some callers hash or compare the id).
        let mut x = seed | 1;
        for i in 0..32 {
            x ^= x >> 30;
            x = x.wrapping_mul(0xbf58476d1ce4e5b9);
            x ^= x >> 27;
            ctx.set_array_element(arr, i, Value::Int((x & 0xff) as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });

    // proto/cipher slot order differs between the shapes; the width is what
    // separates the two conventions. E31: the table (and the off-by-one this
    // comment used to carry — it named a "7-field" and a "3-field" shape that
    // are actually 8 and 4) now lives on `session_cipher_slot`, which both
    // accessors below call instead of re-deriving `>= 7` twice.
    //
    // E12: and NEVER hand back a null String. Both of these are contracted
    // non-null by JSSE, and both used to return `ctx.get_field(...)` raw — so
    // a shape whose slot was never populated (e.g. `http2.rs`'s 6-field
    // `HttpResponse.sslSession()` session, which is allocated and never
    // written) produced a null String from a method that cannot return one,
    // and the caller NPE'd on `.equals`/`.startsWith`. The sentinel is the
    // right answer there because an unpopulated slot IS "nothing negotiated";
    // see `JSSE_NULL_CIPHER_SUITE` for why it is honesty and not invention.
    r.register(cls, "getProtocol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Bound to a `let` on purpose: a closure written directly in a match
        // scrutinee is a temporary whose `&ctx` capture lives to the end of the
        // match, which collides with the `&mut ctx` the sentinel arms need.
        let raw =
            session_proto_slot(ctx.object_num_fields(this)).map(|slot| ctx.get_field(this, slot));
        match raw {
            Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
            _ => {
                let s = ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL);
                Ok(Some(Value::Object(Some(s))))
            }
        }
    });
    r.register(
        cls,
        "getCipherSuite",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // See `getProtocol` above for why this is a `let` and not an
            // inline match scrutinee.
            let raw = session_cipher_slot(ctx.object_num_fields(this))
                .map(|slot| ctx.get_field(this, slot));
            match raw {
                Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
                _ => {
                    let s =
                        ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE);
                    Ok(Some(Value::Object(Some(s))))
                }
            }
        },
    );

    // E12: `isValid` and `getId` answer from ONE predicate, because HotSpot
    // answers them from one piece of state — see `session_has_negotiated`.
    // This used to return `Value::Int(1)` for every shape narrower than 7
    // fields on the reasoning that "the accept session was just negotiated".
    // That reasoning holds for the accept shape and NOT for the NEW-13 shape,
    // which `ssl_security::new13_resolve_socket_session` also mints — with
    // `tls_id = -1` — for a socket that was never connected. So an unconnected
    // `SSLSocket.getSession().isValid()` answered `true` where HotSpot measures
    // `false` (E12-1 §1 arm A).
    //
    // E42: those are now the SAME WIDTH (4), which is why the width alone can
    // never answer this and `session_has_negotiated` must read slot 2. The two
    // shapes are distinguished by the VALUE there — `-1` versus a real stream
    // id — and by nothing else.
    //
    // Deliberately NOT tied to stream liveness: measured (arm G), closing the
    // socket does NOT invalidate the session — it outlives its transport, for
    // resumption — so validity must not be a "is the stream still in the
    // registry" test. Reading the session's own recorded id keeps the answer
    // stable across `close()`.
    //
    // SHADOWING (wave 3): `tls.rs::register_ssl_session` registers the same
    // triple and runs later, so THAT one wins under `--synthetic-jdk`; in the
    // default real-JDK mode (which is what `--jdk-only` runs) THIS one is
    // live — confirmed against `--dump-native-registry`, not source order.
    // Its version was slot-2-only and mis-reported the 3-field accept session;
    // it has been made field-count aware to match this logic. Keep in step.
    //
    // F18 — THE SECOND HALF OF THE PREDICATE, which this VM could not model
    // until `invalidate()` had a writer in this mode. HotSpot's `isValid()` is
    // `sessionId.length() != 0 && !invalidated && ...`; `session_is_valid`
    // composes both halves and is now the one function both modes call. It
    // reduces to `session_has_negotiated` exactly when nothing has ever been
    // invalidated, so this line changes no answer in a process that never
    // calls `invalidate()`. See `session_is_valid` for the measurement on a
    // genuinely negotiated session.
    r.register(cls, "isValid", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(if session_is_valid(ctx, this) {
            1
        } else {
            0
        })))
    });

    // invalidate() -> void
    //
    // F18 (F10-1 NOMINATION 3). This had **no registration in real-JDK mode at
    // all** — `javax.net.ssl.SSLSession` is an interface whose `invalidate()`
    // has no Code attribute, so the shipping mode threw `AbstractMethodError`.
    // It was unreachable in practice only while every session this VM handed
    // out answered `isValid() == false` anyway; F10's minter fix made completed
    // HTTPS handshakes report `isValid() == true`, so a program that checks
    // validity and then invalidates now gets here.
    //
    // Both halves of that nomination land together — the registration AND a
    // state model that makes it mean something at width 4 — because
    // `session_mark_invalidated` records the bit outside the object. See
    // `session_invalidated_table` for why that is not the widening F6-1 §4 and
    // E42-1 §2 reject, and `session_is_valid` for the measured contract.
    r.register(cls, "invalidate", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        session_mark_invalidated(ctx, this);
        Ok(None)
    });

    // getPeerHost() / getPeerPort()
    //
    // F18 — neither had ANY real-JDK-mode registration. `grep -rn '"getPeerHost"'`
    // over `native-builtins/src/` returns only `tls.rs`, whose registrar
    // `register_tls_natives` is reached solely from
    // `register_synthetic_overrides` (`#[cfg(feature = "synthetic-jdk")]`), so
    // in the mode `--jdk-only` runs both were `AbstractMethodError`. E12-1's
    // residual 4 recorded these as *answering the wrong value* — it was reading
    // `build_synthetic_ssl_session`, the producer. The accessor side is worse
    // than wrong; it is absent.
    //
    // MEASURED, HotSpot 25.0.3+9-LTS (`scratchpad/f18/F18SessionContract.java`,
    // three runs byte-identical):
    //
    //   unconnected SSLSocket / pre-handshake SSLEngine   host=null   port=-1
    //   completed HTTPS client handshake                  host=localhost
    //                                                     port=<the server port>
    //   after invalidate()                                unchanged (both)
    //
    // Note HotSpot reports the host the caller ASKED for, not the certificate's
    // subject — `localhost` here, while the leaf certificate is also
    // `CN=localhost`; do not "verify" one against the other.
    //
    // Width handling, which is the trap this family keeps falling into:
    // slot 3 is the peer host on the 6- and 8-field shapes and the ATTRIBUTE
    // MAP on the 4-field one, so a slot read with no width test would return a
    // `java.util.HashMap` through a `()Ljava/lang/String;` descriptor as soon
    // as anything had called `putValue` — the exact defect E31-1 §2 records
    // against `tls.rs`'s copies. The `>= 6` threshold is
    // `session_cipher_slot`'s, deliberately the same number so there is one
    // width boundary in this file and not two.
    //
    // For the 4-field shapes the answer comes from the socket registry via
    // `session_stream_id`, which is where the host and port a client actually
    // dialled are recorded (`servlet::s2_tls_session_info`'s third and fourth
    // components). That is a real answer rather than a fallback, and it is the
    // half E12-1's residual 4 said was missing.
    r.register(cls, "getPeerHost", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) >= 6 {
            // Only a String counts: `tls.rs::init_ssl_session_fields` seeds
            // this slot with `Int(0)`, and `build_synthetic_ssl_session` may
            // leave it unwritten.
            if let v @ Value::Object(Some(_)) = ctx.get_field(this, 3) {
                return Ok(Some(v));
            }
            return Ok(Some(Value::Object(None)));
        }
        // G51: the object-keyed endpoint table, consulted BEFORE the socket
        // registry and AFTER the width branch above. See
        // `session_peer_endpoint_table` for the measured family and for why the
        // answer cannot live in a session slot on this shape.
        //
        // Order matters in one direction only. A recorded row is the endpoint
        // the caller NAMED for this exact session object; the registry lookup
        // below is keyed on a TLS stream id, and the shape that most needs an
        // answer here — the HTTPS client session — carries
        // `net_phase_e::HTTPS_CLIENT_SESSION_MARKER` in slot 2 precisely so
        // that every socket-registry lookup MISSES (see that constant: it
        // cannot be given a real stream id without leaking one registry entry
        // per request). So for that shape the fallback cannot answer by
        // design, and for the accepted server session the registry has no row
        // either — `s2_tls_session_info` reads `servlet`'s native-tls table,
        // and an accepted rustls stream lives in this file's `server_streams`.
        // Same `let`-binding discipline as the fallback below.
        let recorded = session_peer_endpoint(ctx, this)
            .map(|(host, _)| host)
            .filter(|h| !h.is_empty());
        if let Some(host) = recorded {
            let s = ctx.create_string(&host);
            return Ok(Some(Value::Object(Some(s))));
        }
        // Bound to a `let`, and NOT written as `if let Some(id) =
        // session_stream_id(ctx, this)`: the `&*ctx` reborrow in an `if let`
        // scrutinee is a temporary that lives to the end of the block under
        // Rust 2021's drop rules, which would collide with the `&mut ctx` that
        // `create_string` needs inside it. Same hazard `getProtocol` in this
        // file records against a match scrutinee.
        let host = session_stream_id(ctx, this)
            .and_then(servlet::s2_tls_session_info)
            .map(|(_, _, host, _)| host)
            .filter(|h| !h.is_empty());
        if let Some(host) = host {
            let s = ctx.create_string(&host);
            return Ok(Some(Value::Object(Some(s))));
        }
        // HotSpot's measured answer for a session that negotiated nothing.
        Ok(Some(Value::Object(None)))
    });
    r.register(cls, "getPeerPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) >= 6 {
            // `0` means UNWRITTEN, not "port zero". Both producers of a wide
            // shape that populate this slot write `-1` explicitly for "no peer"
            // (`build_synthetic_ssl_session` slot 4, `tls.rs
            // ::init_ssl_session_fields` `SES_PEER_PORT`), and `http2.rs`'s
            // 6-field session writes NO slot at all, so a zero here is the
            // allocator's fill. A connected peer never has port 0 — it is the
            // "any port" value a bind may ask for and never a value a peer
            // reports — so mapping it to HotSpot's measured `-1` cannot mask a
            // real answer.
            if let Value::Int(p) = ctx.get_field(this, 4) {
                return Ok(Some(Value::Int(if p == 0 { -1 } else { p })));
            }
            return Ok(Some(Value::Int(-1)));
        }
        // G51 — the endpoint table, same position and same reasoning as
        // `getPeerHost` above. A recorded row whose port is non-positive is NOT
        // an answer: `record_session_peer_endpoint` accepts a host-only row
        // (nothing writes one today, but the shape is reachable), and falling
        // through to the registry there is right for the same reason an absent
        // row falls through.
        let recorded = session_peer_endpoint(ctx, this)
            .map(|(_, port)| port)
            .filter(|p| *p > 0);
        if let Some(p) = recorded {
            return Ok(Some(Value::Int(p)));
        }
        // Same `let`-binding discipline as `getPeerHost` above. This body has
        // no `&mut ctx` use inside the block today, so the hazard is latent
        // rather than live — which is exactly when it gets introduced.
        let port = session_stream_id(ctx, this)
            .and_then(servlet::s2_tls_session_info)
            .map(|(_, _, _, port)| port)
            .filter(|p| *p != 0);
        match port {
            Some(p) => Ok(Some(Value::Int(p as i32))),
            None => Ok(Some(Value::Int(-1))),
        }
    });

    // getSessionContext() -> SSLSessionContext
    //
    // F18 — registered NOWHERE in the crate, in either mode: `grep -rn
    // '"getSessionContext"'` over `native-builtins/src/` returns zero
    // registrations. Another abstract interface declaration, another
    // `AbstractMethodError`, and the last of the four `javax/net/ssl/SSLSession`
    // doors that had none (the others being `invalidate`, `getPeerHost` and
    // `getPeerPort`, all above).
    //
    // G7 — **this used to answer `null` unconditionally, and the premise the
    // constant rested on was false.** The comment it replaced said "This VM has
    // no `SSLSessionContext` — no session cache, no id-keyed lookup, no
    // timeout". SOURCE-VERIFIED, and it is not so: `net_phase_e
    // ::register_phase_e_networking` allocates a zero-field
    // `javax/net/ssl/SSLSessionContext` carrier for
    // `SSLContext.getClientSessionContext()` / `getServerSessionContext()` and
    // registers the WHOLE interface on it — `getIds()Ljava/util/Enumeration;`,
    // `getSession([B)Ljavax/net/ssl/SSLSession;`, `get/setSessionCacheSize(I)`,
    // `get/setSessionTimeout(I)`. So the object exists, it has a live method
    // surface, and applications already receive it through the other two doors.
    // Answering `null` here was not `--jdk-only` restraint; it was this door
    // disagreeing with its two siblings about whether the VM has a context.
    //
    // MEASURED, HotSpot 25.0.3+9-LTS, `scratchpad/g7/TlsProbe.java`
    // (loopback `SSLServerSocket` + a paired in-memory `SSLEngine`; see
    // `docs/known-issues/jdk-only/G7-1-*.md` §1 for the whole surface):
    //
    // ```text
    //   never-connected SSLSocket session        getSessionContext() = null
    //   fresh SSLEngine session                  getSessionContext() = null
    //   MID-HANDSHAKE (inside checkServerTrusted) getSessionContext() = null
    //   completed handshake                      = SSLSessionContextImpl
    //   resumed handshake                        = SSLSessionContextImpl
    //   HttpsURLConnection.getSSLSession().get() = SSLSessionContextImpl
    //   after invalidate()                       = null
    // ```
    //
    // Four states, and the rule behind them is exactly "is this session IN a
    // context" — a session enters one when its handshake COMPLETES and leaves
    // it when `invalidate()` evicts it (`ctx.getSession(id)` answered
    // SAME-OBJECT before the invalidate and `null` after, same probe). The
    // mid-handshake `null` is the row that matters most here: it is the one
    // state where the session already has its 32-byte id, its cipher suite and
    // `isValid() == true` and still has no context, so a gate on validity alone
    // would get it wrong.
    //
    // The two gates below are that rule, and the second one is the gate this
    // merge moved OFF `getId()`:
    //
    //   * `session_is_valid` = `session_has_negotiated && !invalidated`. It
    //     covers the never-negotiated states (slot-2 sentinel `-1`, or the
    //     engine shape's `isValid` flag `0`) and the invalidated one.
    //   * `session_is_negotiated` — the `negotiated_session_keys` membership
    //     `engine_session_for` writes ONLY in the handshake-completed epoch —
    //     covers mid-handshake. It is consulted for the 7/8-field engine shape
    //     alone, because that is the only shape whose object can be handed out
    //     while a handshake is still running (`SSLEngineImpl
    //     .getHandshakeSession` above). The socket door answers `null` for its
    //     whole handshake window, so no width-4 session is observable there.
    //
    // Both predicates already existed. `getId()` used to stack them and was
    // wrong for it — see its registration — and moving the second one here is
    // what keeps `negotiated_session_keys` load-bearing instead of leaving it a
    // write-only set.
    //
    // WHY A CARRIER AND NOT A FABRICATION. The interface's own escape hatch
    // ("This context may be unavailable in some environments, in which case
    // this method returns null" — `javax/net/ssl/SSLSession.getSessionContext`)
    // is still the right answer for the three states above that answer `null`.
    // What it does not license is claiming unavailability for a completed
    // handshake while `SSLContext.getClientSessionContext()` hands the same
    // application a context object one call away. The carrier this mints is
    // that same object, not a new shape: zero fields, `javax/net/ssl/
    // SSLSessionContext`, so every method on it lands on net_phase_e's
    // registrations rather than on an `AbstractMethodError`.
    //
    // TWO RESIDUALS, stated rather than hidden — both are net_phase_e's to
    // close and are NOMINATED in the record:
    //   1. `getIds()` answers an empty enumeration and `getSession(id)` answers
    //      `null`, where HotSpot's context lists this session and returns the
    //      SAME object for its id. A context that does not contain the session
    //      that pointed at it is a real inconsistency; it is bounded (rustls
    //      owns the cache and exposes no enumeration) and it is strictly less
    //      wrong than `null`, which fails a plain non-null check.
    //   2. `getSessionCacheSize()`/`getSessionTimeout()` read net_phase_e's
    //      `ssc_side_table` under an ORPHAN key, so they answer its default
    //      `0`/`0` where HotSpot measured `20480`/`86400`. `ssc_bind` is
    //      private to that file, so this carrier cannot be bound from here.
    r.register(
        cls,
        "getSessionContext",
        "()Ljavax/net/ssl/SSLSessionContext;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Hoisted, not chained: `session_is_valid` takes `&dyn` and
            // `session_is_negotiated` `&mut dyn`, and this file already records
            // (see `getPeerHost`, and `getId` below) how easily a reborrow in a
            // compound scrutinee collides with the `&mut` beside it.
            let valid = session_is_valid(ctx, this);
            let engine_shape = ctx.object_num_fields(this) >= 7;
            if !valid || (engine_shape && !session_is_negotiated(ctx, this)) {
                return Ok(Some(Value::Object(None)));
            }
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSessionContext", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // creation / last-accessed time: the 8-field engine session stores a
    // millis timestamp in slot 5; the narrower shapes have no slot for one.
    //
    // E12: the narrow shapes used to answer `0`, and HotSpot never does —
    // measured (E12-1 §1), `getCreationTime()` is a real epoch value in EVERY
    // state including the null session, and `0` is exactly the value
    // application code reads as "there is no session". Answer a real epoch.
    //
    // RESIDUAL, stated rather than hidden: HotSpot's value is fixed at
    // construction, so two reads of one session agree; this one is `now` on
    // every call for the narrow shapes, so two reads disagree by the elapsed
    // millis. Making it stable needs a slot to stash it in (the NEW-13 shape
    // has none — its slot 3 is the attribute map, not a timestamp). A
    // drifting real timestamp is still strictly better than a stable
    // impossible one: age arithmetic (`now - creationTime`), which is what
    // every consumer actually does, goes from "56 years old" to "~0 ms old".
    //
    // E42 did NOT change this. The NEW-13 widening stops at 4 and this gate is
    // `> 5`, so the widened shape still answers `now` on every call. E22-1's
    // NOMINATION C asked for a widening that carried a creation-time slot and
    // this is not that widening — reaching slot 5 means width >= 6, which is
    // `tls.rs`'s cipher-first convention and would shift slots 0/1/2 under
    // every reader in this file.
    r.register(cls, "getCreationTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 5 {
            Ok(Some(ctx.get_field(this, 5)))
        } else {
            Ok(Some(Value::Long(crate::epoch_millis_now())))
        }
    });
    r.register(cls, "getLastAccessedTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // A session reused by a later handshake was last accessed then, not
        // when it was created (`session_last_accessed_table`).
        let k = gc_stable_objref_key(ctx, this);
        if let Some(t) = session_last_accessed_table().lock().get(&k).copied() {
            return Ok(Some(Value::Long(t)));
        }
        if ctx.object_num_fields(this) > 5 {
            Ok(Some(ctx.get_field(this, 5)))
        } else {
            Ok(Some(Value::Long(crate::epoch_millis_now())))
        }
    });

    // getValue/putValue/removeValue/getValueNames — the JSSE session-attribute
    // API. `SSLSession` is an interface with no default body for any of these,
    // so an un-intercepted call throws AbstractMethodError. Jetty's
    // `SecureRequestCustomizer.retrieveSni()` calls `getValue()`/`putValue()`
    // on every SSL request to cache the resolved SNI host, so this previously
    // failed every HTTPS request that reached `SecureRequestCustomizer`
    // (`JettyServletWebServerFactoryTests`/`JettyReactiveWebServerFactoryTests`,
    // both 500s with this exact AbstractMethodError). Backed by a real
    // `java.util.HashMap` stored in the session's own LAST field (slot
    // `num_fields - 1`, added to both the 4-field and 8-field shapes above)
    // rather than a native side-table keyed by object address — normal GC
    // root-scanning of the (live, reachable) session object keeps arbitrary
    // attribute VALUES alive for free, avoiding the class of GC-unstable
    // objref-key bug this file's other side tables have hit.
    r.register(
        cls,
        "getValue",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            // MEASURED: IllegalArgumentException, SINGULAR "argument".
            if matches!(args.get(1), None | Some(Value::Object(None))) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "argument can not be null".into(),
                }
                .into());
            }
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            // E31: `sslsess_attrs_slot`, not `num_fields - 1` — see its doc.
            let slot = match sslsess_attrs_slot(ctx, this) {
                Some(s) => s,
                None => return Ok(Some(Value::Object(None))),
            };
            let map = match ctx.get_field(this, slot) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.invoke(
                "java/util/HashMap",
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(map)), name],
            )
        },
    );
    r.register(
        cls,
        "putValue",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let value = args.get(2).copied().unwrap_or(Value::Object(None));
            if matches!(name, Value::Object(None)) || matches!(value, Value::Object(None)) {
                return Err(RuntimeError::IllegalArgumentException {
                    // MEASURED 2026-08-13 (scratchpad/orch/Three.java): JSSE
                    // VALIDATES here rather than dereferencing, so the KIND is
                    // IllegalArgumentException, not NPE -- and the text is the
                    // JDK's, not the invented "name and value must not be null".
                    // Note the PLURAL: putValue says "arguments", getValue and
                    // removeValue say "argument". One letter, two messages.
                    message: "arguments can not be null".into(),
                }
                .into());
            }
            // E31: a shape with no dedicated attribute slot must NOT fall back
            // to `num_fields - 1` — on the then-3-field NEW-13 shape that slot
            // was the stream id, and overwriting it with a HashMap flips
            // `session_has_negotiated` to `true`, which is exactly how a
            // `putValue` used to resurrect the 32-byte fabricated id and
            // `isValid() == true` on a session that never handshaked. No-op
            // instead; see `sslsess_attrs_slot`.
            //
            // E42: that shape is now 4 fields with a dedicated slot 3, so this
            // door ROUND-TRIPS for it rather than no-opping — which is the
            // whole point of the widening (Jetty's `SecureRequestCustomizer
            // .retrieveSni()` does `getValue()` then `putValue()` on every SSL
            // request). The `None` arm is now reached only by the 6-field
            // shape. Do not "simplify" it back to `num_fields - 1`: on that
            // shape the last slot is `tls.rs::SES_CREATION_TIME`.
            let slot = match sslsess_attrs_slot(ctx, this) {
                Some(s) => s,
                None => return Ok(None),
            };
            let map = sslsess_attrs_map(ctx, this, slot)?;
            let old = ctx.invoke(
                "java/util/HashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(map)), name, value],
            )?;
            // JSSE unbinds the value being replaced before binding the new one.
            if let Some(old @ Value::Object(Some(_))) = old {
                fire_session_binding(ctx, this, name, old, false);
            }
            fire_session_binding(ctx, this, name, value, true);
            Ok(None)
        },
    );
    r.register(cls, "removeValue", "(Ljava/lang/String;)V", |ctx, args| {
        // MEASURED: IllegalArgumentException, SINGULAR "argument".
        if matches!(args.get(1), None | Some(Value::Object(None))) {
            return Err(RuntimeError::IllegalArgumentException {
                message: "argument can not be null".into(),
            }
            .into());
        }
        let this = obj_arg(args, 0)?;
        let name = args.get(1).copied().unwrap_or(Value::Object(None));
        // E31: `sslsess_attrs_slot`, not `num_fields - 1` — see its doc.
        let slot = match sslsess_attrs_slot(ctx, this) {
            Some(s) => s,
            None => return Ok(None),
        };
        let map = match ctx.get_field(this, slot) {
            Value::Object(Some(m)) => m,
            _ => return Ok(None),
        };
        let old = ctx.invoke(
            "java/util/HashMap",
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(map)), name],
        )?;
        if let Some(old @ Value::Object(Some(_))) = old {
            fire_session_binding(ctx, this, name, old, false);
        }
        Ok(None)
    });
    r.register(
        cls,
        "getValueNames",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let empty = |ctx: &mut dyn NativeContext| {
                Ok(Some(Value::Object(Some(
                    ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0),
                ))))
            };
            // E31: `sslsess_attrs_slot`, not `num_fields - 1` — see its doc. On the
            // then-3-field shape the old spelling read the STREAM ID as a map
            // reference; it missed (an `Int`) and answered empty, so this arm was
            // unchanged in effect and changed in correctness. E42: that shape is
            // width 4 now and slot 3 really is the map, so this door reports the
            // names a `putValue` actually stored.
            let slot = match sslsess_attrs_slot(ctx, this) {
                Some(s) => s,
                None => return empty(ctx),
            };
            let map = match ctx.get_field(this, slot) {
                Value::Object(Some(m)) => m,
                _ => return empty(ctx),
            };
            let key_set = match ctx.invoke(
                "java/util/HashMap",
                "keySet",
                "()Ljava/util/Set;",
                &[Value::Object(Some(map))],
            )? {
                Some(Value::Object(Some(s))) => s,
                _ => return empty(ctx),
            };
            // `keySet()`'s runtime type is `HashMap$KeySet`, not `HashSet` — resolve
            // `toArray`'s declaring class dynamically rather than guessing a name,
            // since a wrong static class name here would use the wrong field/vtable
            // layout for the dispatch.
            let key_set_class = ctx.class_id_of_object(key_set);
            let key_set_class_name = match ctx.class_name_of_id(key_set_class) {
                Some(n) => n,
                None => return empty(ctx),
            };
            let raw_arr = match ctx.invoke(
                &key_set_class_name,
                "toArray",
                "()[Ljava/lang/Object;",
                &[Value::Object(Some(key_set))],
            )? {
                Some(Value::Object(Some(arr))) => arr,
                _ => return empty(ctx),
            };
            // `Collection.toArray()` reifies as `Object[]`, not `String[]` — real
            // JDK's own `SSLSessionImpl.getValueNames()` has the same mismatch and
            // copies into a freshly-typed array rather than returning it directly.
            // Mirror that: allocate our own array (same `ClassId::new(0)` "generic
            // String[]" convention already used elsewhere in this file, e.g.
            // `getEnabledProtocols`) and copy each key across.
            let len = ctx.array_length(raw_arr);
            let out = ctx.new_ref_array(cratonvm_types::ClassId::new(0), len);
            for i in 0..len {
                ctx.set_array_element(out, i, ctx.get_array_element(raw_arr, i));
            }
            Ok(Some(Value::Object(Some(out))))
        },
    );
}

/// Lazily allocate (and cache in the session's own last field) the
/// `java.util.HashMap` backing `SSLSession.putValue`/`getValue`/etc — see the
/// doc comment on its registration in `register_ssl_session_real`.
///
/// E31: `slot` is now a PARAMETER, resolved by `sslsess_attrs_slot` at the one
/// caller that may write. It used to compute `num_fields - 1` itself, which is
/// the attribute slot on only two of five shapes — see `sslsess_attrs_slot`
/// for what the other three lost. Taking it as an argument is what makes the
/// "does this shape even have an attribute slot?" question unskippable rather
/// than something a future caller can forget to ask.
fn sslsess_attrs_map(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slot: usize,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    if let Value::Object(Some(map)) = ctx.get_field(this, slot) {
        return Ok(map);
    }
    let this_pin = ctx.pin_native_root(this);
    let map = match ctx.new_object_initialized("java/util/HashMap", "()V", &[])? {
        Some(Value::Object(Some(m))) => m,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Err(RuntimeError::OutOfMemoryError {
                message: "SSLSession: could not allocate attribute map".into(),
            }
            .into());
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(this, slot, Value::Object(Some(map)));
    Ok(map)
}

/// WP5.4 — register ALPN-related natives on SSLParameters. ALPN propagation
/// from `SSLParameters` → `SSLEngineImpl` is wired in `register_apply_parameters`
/// (see `register_sslengine_real`).
pub fn register_alpn_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_alpn_on_parameters(r);
    r.set_category(__prev_cat);
}

#[allow(dead_code)]
fn _wp51_keep_symbols_live() {
    let _ = engine_negotiated_alpn_internal;
}
