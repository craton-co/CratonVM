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

use crate::alloc_concurrent_synthetic;
use crate::servlet;

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
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
) -> Vec<Vec<u8>> {
    let key = ctx_obj_key(ctx, context);
    ctx_trust_roots_table()
        .lock()
        .get(&key)
        .map(|roots| roots.root_ders.clone())
        .unwrap_or_default()
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

fn ctx_obj_key(ctx: &mut dyn NativeContext, obj: ObjectRef) -> u64 {
    crate::gc_stable_lock_key(ctx, obj) as u64
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
) {
    let key = ctx_obj_key(ctx, ctx_obj);
    let mut list = Vec::new();
    if let Some(arr) = tms_array {
        let len = ctx.array_length(arr);
        for i in 0..len {
            if let Value::Object(Some(tm)) = ctx.get_array_element(arr, i) {
                list.push(tm);
            }
        }
    }
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
        eprintln!(
            "[dbg-tls-auth] attach_trust_managers_to_ctx key={} tms_array_present={} count={}",
            key,
            tms_array.is_some(),
            list.len()
        );
    }
    let mut table = ctx_trust_managers_table().lock();
    if list.is_empty() {
        table.remove(&key);
    } else {
        table.insert(key, list);
    }
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
) -> Option<u64> {
    let key = ctx_obj_key(ctx, ctx_obj);
    if ctx_trust_managers_table().lock().contains_key(&key) {
        Some(key)
    } else {
        None
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
) {
    let key = ctx_obj_key(ctx, ctx_obj);
    let mut list = Vec::new();
    if let Some(arr) = kms_array {
        let len = ctx.array_length(arr);
        for i in 0..len {
            if let Value::Object(Some(km)) = ctx.get_array_element(arr, i) {
                list.push(km);
            }
        }
    }
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
) {
    let key = ctx_obj_key(ctx, ctx_obj);
    let pending = take_pending_km_identity();
    if let Some(ident) = resolved_km_identity.or(pending) {
        if std::env::var_os("CRATONVM_DBG_TLS_AUTH").is_some() {
            eprintln!(
                "[dbg-tls-auth] attach_pending_identity_to_ctx key={} STORING km identity key_pem_len={} cert_pem_len={}",
                key,
                ident.1.len(),
                ident.0.len()
            );
        }
        ctx_identity_table().lock().insert(key, ident);
    } else if std::env::var_os("CRATONVM_DBG_TLS_AUTH").is_some() {
        eprintln!(
            "[dbg-tls-auth] attach_pending_identity_to_ctx key={} NO pending km identity to store",
            key
        );
    }
    if let Some(roots) = take_pending_tm_trust_roots() {
        if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
            eprintln!(
                "[dbg-tls-auth] attach_pending_identity_to_ctx key={} storing {} roots",
                key,
                roots.root_ders.len()
            );
        }
        ctx_trust_roots_table().lock().insert(key, roots);
    } else if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
        eprintln!(
            "[dbg-tls-auth] attach_pending_identity_to_ctx key={} NO pending roots to store",
            key
        );
    }
}

/// Look up the identity previously associated with an `SSLContext` object.
pub(crate) fn ctx_identity(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
) -> Option<(String, String)> {
    let key = ctx_obj_key(ctx, ctx_obj);
    let trust_roots = ctx_trust_roots_table().lock().get(&key).cloned();
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
        eprintln!(
            "[dbg-tls-auth] ctx_identity key={} trust_roots={:?}",
            key,
            trust_roots.as_ref().map(|r| r.root_ders.len())
        );
    }
    set_selected_context_trust_roots(trust_roots);
    ctx_identity_table().lock().get(&key).cloned()
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
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
        if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
    let key = ctx_obj_key(ctx, ctx_obj);
    let identity = ctx_identity(ctx, ctx_obj);
    build_engine_client_config_with_identity_ciphers(
        &["http/1.1"],
        identity
            .as_ref()
            .map(|(cert, key)| (cert.as_str(), key.as_str())),
        Some(key),
        Some(key),
        enabled_ciphers,
    )
}

/// As `build_engine_client_config_with_identity`, but additionally restricts
/// the negotiable cipher suites to `enabled_ciphers` when non-empty. Used by
/// `SSLSocket.setEnabledCipherSuites` (net_phase_e.rs) to reconnect a socket
/// created via `SSLSocketFactory.createSocket` under a real cipher
/// restriction — callers rely on the JDK contract that a socket rejects the
/// handshake when restricted to a suite the server doesn't support (e.g.
/// Tomcat's `TesterSupport.ClientSSLSocketFactory`).
pub(crate) fn build_engine_client_config_with_identity_ciphers(
    alpn: &[&str],
    client_identity: Option<(&str, &str)>,
    km_ctx_key: Option<u64>,
    trust_managers_ctx_key: Option<u64>,
    enabled_ciphers: &[String],
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
    let provider = cipher_provider_for(enabled_ciphers);
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
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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

fn capture_huc_trust_managers_ctx_key(ctx: &mut dyn NativeContext, ctx_obj: ObjectRef) {
    let key = ctx_obj_key(ctx, ctx_obj);
    let has_managers = ctx_trust_managers_table()
        .lock()
        .get(&key)
        .map(|managers| !managers.is_empty())
        .unwrap_or(false);
    *huc_default_tm_ctx_key_slot().lock() = has_managers.then_some(key);
}

pub(crate) fn huc_default_trust_managers_ctx_key() -> Option<u64> {
    *huc_default_tm_ctx_key_slot().lock()
}

/// `SSLContext.getSocketFactory()` calls this alongside
/// `set_huc_default_client_identity` (same reliable per-context capture
/// point — see that call site's doc). Clears the slot when this context has
/// no captured `KeyManager`s at all, so a plain non-mTLS client (or one
/// whose `SSLContext.init` passed a null/empty `KeyManager[]`) keeps falling
/// back to `client_identity`/no-client-auth instead of spuriously trying (and
/// failing) to consult an empty resolver.
pub(crate) fn capture_huc_key_managers_ctx_key(ctx: &mut dyn NativeContext, ctx_obj: ObjectRef) {
    let key = ctx_obj_key(ctx, ctx_obj);
    let has_kms = ctx_key_managers_table().lock().contains_key(&key);
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
        eprintln!(
            "[dbg-tls-auth] capture_huc_key_managers_ctx_key key={} has_kms={}",
            key, has_kms
        );
    }
    set_huc_default_key_managers_ctx_key(if has_kms { Some(key) } else { None });
}

/// Capture all TLS state for the Java SSLContext supplying HttpsURLConnection.
/// This also runs for anonymous clients: `ctx_identity` transfers scoped trust
/// roots even when it returns no client certificate, and every context needs a
/// stable ClientConfig to retain TLS 1.3 tickets across URL requests.
pub(crate) fn capture_huc_ssl_context(ctx: &mut dyn NativeContext, ctx_obj: ObjectRef) {
    let ident = ctx_identity(ctx, ctx_obj);
    set_huc_default_client_identity(ident);
    capture_huc_key_managers_ctx_key(ctx, ctx_obj);
    capture_huc_trust_managers_ctx_key(ctx, ctx_obj);

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
}

/// Capture an instance factory without changing the process-default TLS
/// policy. HttpsURLConnection's instance setter is scoped to one connection;
/// keeping its config in the default slot made a later plain connection accept
/// the previous connection's permissive TrustManager.
pub(crate) fn capture_huc_ssl_context_for_connection(
    ctx: &mut dyn NativeContext,
    connection: ObjectRef,
    ctx_obj: ObjectRef,
) {
    let default_identity = huc_default_identity_slot().lock().clone();
    let default_roots = huc_default_trust_roots_slot().lock().clone();
    let default_config = huc_default_client_config_slot().lock().clone();
    let default_km = *huc_default_km_ctx_key_slot().lock();
    let default_tm = *huc_default_tm_ctx_key_slot().lock();

    capture_huc_ssl_context(ctx, ctx_obj);
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
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
    if std::env::var_os("CRATONVM_DBG_TLS_HS").is_some() {
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

impl SniCertResolver {
    /// Build a `CertifiedKey` from PEM blobs. Uses rustls's ring-backed
    /// signer, which covers RSA 2048/3072/4096 and ECDSA P-256/P-384.
    fn certified_key_from_pem(cert_pem: &str, key_pem: &str) -> Result<Arc<CertifiedKey>, String> {
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
    pub(crate) stream: StreamOwned<ClientConnection, TcpStream>,
    pub(crate) peer_host: String,
    pub(crate) peer_port: u16,
    pub(crate) negotiated_protocol: String,
    pub(crate) negotiated_cipher: String,
    pub(crate) negotiated_alpn: Option<String>,
}

pub(crate) struct TlsServerStreamEntry {
    pub(crate) stream: TlsServerStream,
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
    )
}

/// As `build_server_config_single_cert_ex`, but restricts the negotiable
/// cipher suites to `enabled_ciphers` (Java `SSLEngine.setEnabledCipherSuites`
/// names) when non-empty — an empty list keeps the unrestricted `ring`
/// default, identical to `build_server_config_single_cert_ex`.
pub(crate) fn build_server_config_single_cert_ex_ciphers(
    cert_pem: &str,
    key_pem: &str,
    alpn_protocols: &[&str],
    require_client_cert: bool,
    optional_client_cert: bool,
    client_ca_pem: Option<&str>,
    enabled_ciphers: &[String],
) -> Result<Arc<ServerConfig>, String> {
    let chain = parse_cert_chain_pem(cert_pem)?;
    let key = parse_private_key_pem(key_pem)?;

    let provider = cipher_provider_for(enabled_ciphers);
    let builder = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("with_safe_default_protocol_versions failed: {}", e))?;
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
/// `docs/internal/fixed-suite-bugs/tls-ocsp-clientcert-validation-not-enforced-FIXED.md`.
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
}

impl rustls::client::danger::ServerCertVerifier for PassthroughServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
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
    )
}

/// Provider-aware implementation of `build_client_config_ex`.  A distinct
/// provider is required when Java `SSLParameters` narrows the allowed cipher
/// suites, including for the custom-verifier and Java-KeyManager branches.
fn build_client_config_ex_with_provider(
    roots: RootCertStore,
    alpn_protocols: &[&str],
    client_auth: ClientAuthMode<'_>,
    revocation: Option<crate::x509_manager::RevocationConfig>,
    use_java_trust_manager: bool,
    provider: Arc<rustls::crypto::CryptoProvider>,
) -> Result<Arc<ClientConfig>, String> {
    let builder = if use_java_trust_manager {
        let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> =
            Arc::new(PassthroughServerCertVerifier {
                algorithms: provider.signature_verification_algorithms.clone(),
            });
        ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("with_safe_default_protocol_versions failed: {e}"))?
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
                ClientConfig::builder_with_provider(provider.clone())
                    .with_safe_default_protocol_versions()
                    .map_err(|e| format!("with_safe_default_protocol_versions failed: {e}"))?
                    .dangerous()
                    .with_custom_certificate_verifier(verifier)
            }
            None => ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("with_safe_default_protocol_versions failed: {e}"))?
                .with_root_certificates(roots),
        }
    };
    let mut config = match client_auth {
        ClientAuthMode::Resolver(resolver) => builder.with_client_cert_resolver(resolver),
        ClientAuthMode::Fixed(Some((cert_pem, key_pem))) => {
            let chain = parse_cert_chain_pem(cert_pem)?;
            let key = parse_private_key_pem(key_pem)?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| format!("with_client_auth_cert failed: {}", e))?
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

/// RAII guard returned by `set_active_native_context`; clears the
/// thread-local on drop (including on an early return/`?` inside the
/// handshake loop), so the raw pointer never outlives the native call frame
/// that created it.
pub(crate) struct ActiveNativeContextGuard {
    _private: (),
}

impl Drop for ActiveNativeContextGuard {
    fn drop(&mut self) {
        ACTIVE_TLS_NATIVE_CTX.with(|c| c.set(None));
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
    ACTIVE_TLS_NATIVE_CTX.with(|c| c.set(Some(ptr)));
    ActiveNativeContextGuard { _private: () }
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
fn build_issuer_principals(ctx: &mut dyn NativeContext, root_hint_subjects: &[&[u8]]) -> ObjectRef {
    let dn_strings: Vec<String> = root_hint_subjects
        .iter()
        .filter_map(|der| crate::security_manager::x509::parse_name_dn(der).ok())
        .collect();
    let cls_id = ctx
        .ensure_class_initialized("java/security/Principal")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let arr = ctx.new_ref_array(cls_id, dn_strings.len());
    for (i, dn) in dn_strings.iter().enumerate() {
        let princ = alloc_concurrent_synthetic(ctx, "javax/security/auth/x500/X500Principal", 1);
        let s = ctx.create_string(dn);
        ctx.set_field(princ, 0, Value::Object(Some(s)));
        ctx.set_array_element(arr, i, Value::Object(Some(princ)));
    }
    arr
}

fn materialize_java_string_array(ctx: &mut dyn NativeContext, items: &[String]) -> ObjectRef {
    let cls_id = ctx
        .ensure_class_initialized("java/lang/String")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let arr = ctx.new_ref_array(cls_id, items.len());
    for (i, s) in items.iter().enumerate() {
        let js = ctx.create_string(s);
        ctx.set_array_element(arr, i, Value::Object(Some(js)));
    }
    arr
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
            ctx.class_name_of_id(ctx.class_id_of_object(*exc))
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
    ) -> Option<Arc<CertifiedKey>> {
        let dbg = std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok();
        let mut km_list = ctx_key_managers_table()
            .lock()
            .get(&self.km_ctx_key)?
            .clone();
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
            return None;
        }
        let key_types = key_types_from_sigschemes(sigschemes);
        if dbg {
            eprintln!(
                "[dbg-tls-auth] JavaKeyManagerResolver key_types={:?}",
                key_types
            );
        }
        let key_type_arr = materialize_java_string_array(ctx, &key_types);
        let issuers_arr = build_issuer_principals(ctx, root_hint_subjects);

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
                let args = [
                    Value::Object(Some(key_type_arr)),
                    Value::Object(Some(issuers_arr)),
                    Value::Object(None),
                ];
                let mut choose_result = ctx.invoke_virtual(
                    km_obj,
                    "chooseClientAlias",
                    "([Ljava/lang/String;[Ljava/security/Principal;Ljava/net/Socket;)Ljava/lang/String;",
                    &args,
                );
                let mut retry_attempt = 0;
                while is_abstract_method_error(ctx, &choose_result) && retry_attempt < 3 {
                    retry_attempt += 1;
                    std::thread::yield_now();
                    std::thread::sleep(std::time::Duration::from_millis(5 * retry_attempt));
                    km_list[i] = ctx.read_native_pin(pin, km_list[i]);
                    let km_obj = km_list[i];
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

                let km_id = crate::x509_manager::km_id_from_private_key_mirror(ctx, pk_obj);
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
                let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
                match CertifiedKey::from_der(cert_chain, key, &self.provider) {
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
        result
    }
}

impl ResolvesClientCert for JavaKeyManagerResolver {
    fn resolve(
        &self,
        root_hint_subjects: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        let dbg = std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok();
        if dbg {
            eprintln!(
                "[dbg-tls-auth] JavaKeyManagerResolver::resolve CALLED km_ctx_key={} root_hint_subjects={}",
                self.km_ctx_key,
                root_hint_subjects.len()
            );
        }
        let out = with_active_native_context(|ctx| {
            self.resolve_via_java(ctx, root_hint_subjects, sigschemes)
        });
        if dbg && out.is_none() {
            eprintln!("[dbg-tls-auth] JavaKeyManagerResolver::resolve NO active native context");
        }
        out.flatten()
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
        if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
            eprintln!(
                "[dbg-tls-auth] JavaKeyManagerResolver::has_certs CALLED km_ctx_key={} -> {}",
                self.km_ctx_key, out
            );
        }
        out
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
        _ => return None,
    })
}

/// `ring`'s default `CryptoProvider`, augmented with the T-CBC.1 CBC-mode
/// TLS1.2 suites (`crate::t27_tls_cbc`) that `ring` itself never implements —
/// see `docs/known-issues/springboot/rustls-cbc-cipher-suites-not-supported.md`.
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
    restricted
        .cipher_suites
        .retain(|cs| wanted.contains(&cs.suite()));
    if restricted.cipher_suites.is_empty() {
        return Arc::new(cbc_augmented_default_provider());
    }
    Arc::new(restricted)
}

/// True if at least one of `ciphers` maps to a real rustls `CipherSuite` (see
/// `java_cipher_name_to_suite`). Classic TLS 1.2 `TLS_DHE_RSA_*` names never
/// map — rustls has never implemented finite-field DHE key exchange in any of
/// its crypto providers (`ring`/`aws-lc-rs` only ship ECDHE + TLS 1.3), which
/// is a real upstream library limitation, not an oversight here. Callers that
/// would otherwise silently fall back to an unrestricted connection (see
/// `cipher_provider_for`) should use this to detect that case up front and
/// leave an existing connection alone instead of tearing it down for a
/// restriction that cannot actually be enforced through rustls.
pub(crate) fn any_cipher_mappable(ciphers: &[String]) -> bool {
    ciphers
        .iter()
        .any(|n| java_cipher_name_to_suite(n).is_some())
}

/// A `ClientCertVerifier` that accepts any structurally-valid, correctly
/// SIGNED client certificate WITHOUT validating its chain against a trust
/// anchor. Used exclusively when Tomcat's `trustManagerClassName` mechanism
/// is configured: that feature's whole point is to delegate the trust
/// decision to a Java `TrustManager` class INSTEAD OF a keystore-backed
/// truststore, so there is no CA data here for `WebPkiClientVerifier` to
/// build a `RootCertStore` from.
///
/// Accepting a certificate here does NOT mean the connection is ultimately
/// trusted — it only means the client proved possession of the leaf
/// certificate's private key (`verify_tls12/13_signature` still do real
/// cryptographic signature verification via the same webpki primitives
/// `WebPkiClientVerifier` uses). The actual trust decision is made
/// afterwards, synchronously, by `engine_run_trust_check` calling the real
/// Java `TrustManager.checkClientTrusted` once the handshake completes —
/// which aborts the connection (`SSLHandshakeException`) on rejection. This
/// verifier must therefore ONLY be selected when a Java `TrustManager` is
/// actually registered for the owning engine (see `engine_begin`'s
/// `use_passthrough_client_verifier` check) — never as a general fallback for
/// "no truststore configured", which would be a fail-open regression.
#[derive(Debug)]
struct PassthroughClientCertVerifier {
    mandatory: bool,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::server::danger::ClientCertVerifier for PassthroughClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }
    fn client_auth_mandatory(&self) -> bool {
        self.mandatory
    }
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
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
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
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
) -> Result<Arc<ServerConfig>, String> {
    let chain = parse_cert_chain_pem(cert_pem)?;
    let key = parse_private_key_pem(key_pem)?;
    let provider = cipher_provider_for(enabled_ciphers);
    let algorithms = provider.signature_verification_algorithms;
    let verifier: Arc<dyn rustls::server::danger::ClientCertVerifier> =
        Arc::new(PassthroughClientCertVerifier {
            mandatory: require_client_cert,
            algorithms,
        });
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("with_safe_default_protocol_versions failed: {}", e))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .map_err(|e| format!("ServerConfig with_single_cert failed: {}", e))?;
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
    let provider = cipher_provider_for(enabled_ciphers);
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("with_safe_default_protocol_versions failed: {}", e))?
        .with_root_certificates(roots);
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
                .write_tls(&mut stream.sock)
                .map_err(|e| format!("handshake write: {}", e))?;
        }
        if stream.conn.wants_read() {
            let n = stream
                .conn
                .read_tls(&mut stream.sock)
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
///
/// FIX (h2-testnetutils-accept-close-deadlock): the previous version called
/// the blocking `TcpListener::accept()` *through* the `sreg()`-guarded
/// listener entry, i.e. with `sreg()`'s mutex held for the full duration of
/// the wait for a peer connection — which can be indefinite (H2's
/// `TestNetUtils.testFrequentConnections` starts a background `Task` thread
/// looping `serverSocket.accept()`, and its client-side workers can all
/// finish/bail without ever connecting, e.g. after a *different*, since-fixed
/// bug — see `bug-h2-netutils-dsa-privatekey-tls-unsupported.md` — left them
/// throwing before they ever reached the network). The test's own `finally`
/// block then calls `serverSocket.close()` from the main thread, which needs
/// that SAME mutex (`rustls_listener_close`) to remove the listener entry —
/// permanently deadlocked against the accept thread that can never release it
/// while blocked in the kernel. Fixed by only holding `sreg()` briefly (to
/// clone the `TcpListener` handle and the `closed` flag), then polling
/// `accept()` non-blockingly outside the lock so a `close()` call can always
/// acquire the mutex immediately and is noticed within one poll interval.
pub(crate) fn rustls_server_accept(listener_id: i32) -> Result<i32, String> {
    let debug_hs = std::env::var_os("CRATONVM_DBG_TLS_HS").is_some();
    // Step 1: pop the config + a cloned tcp listener handle + the closed
    // flag, then accept *without* the mutex held so long handshakes (or a
    // long wait for a peer that never connects) don't stall every other TLS
    // operation, and so `close()` is never blocked behind this wait.
    let (config, tcp_listener, closed) = {
        let reg = sreg().lock();
        let entry = reg
            .listeners
            .get(&listener_id)
            .ok_or_else(|| format!("no such SSLServerSocket id: {}", listener_id))?;
        let cloned = entry
            .listener
            .try_clone()
            .map_err(|e| format!("listener try_clone failed: {e}"))?;
        (entry.config.clone(), cloned, entry.closed.clone())
    };

    tcp_listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener set_nonblocking failed: {e}"))?;
    let (tcp, _peer) = loop {
        match tcp_listener.accept() {
            Ok(pair) => break pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if closed.load(Ordering::SeqCst) {
                    return Err("listener closed".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(format!("accept failed: {}", e)),
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

    let (stream, sni_hostname, negotiated_protocol, negotiated_cipher, negotiated_alpn) =
        match config {
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
                        stream
                            .conn
                            .read_tls(&mut stream.sock)
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
                        stream
                            .conn
                            .write_tls(&mut stream.sock)
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
                let protocol = match stream.conn.protocol_version() {
                    Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
                    Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
                    _ => "TLS",
                }
                .to_string();
                let cipher = stream
                    .conn
                    .negotiated_cipher_suite()
                    .map(|cs| format!("{:?}", cs.suite()))
                    .unwrap_or_else(|| "UNKNOWN".to_string());
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
                (
                    TlsServerStream::Native(stream),
                    None,
                    "TLSv1.2".to_string(),
                    "UNKNOWN".to_string(),
                    None,
                )
            }
            #[cfg(unix)]
            TlsServerConfig::LegacyDsa(acceptor) => {
                let stream = acceptor
                    .accept(tcp)
                    .map_err(|e| format!("legacy DSA TLS server handshake: {e}"))?;
                (
                    TlsServerStream::LegacyDsa(stream),
                    None,
                    "TLSv1.2".to_string(),
                    "UNKNOWN".to_string(),
                    None,
                )
            }
        };

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
    let debug_srv = std::env::var_os("CRATONVM_DBG_TLS_SRV").is_some();
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
                .read_tls(&mut stream.sock)
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
                .write_tls(&mut stream.sock)
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
            .write_tls(&mut stream.sock)
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
    let mut reg = sreg().lock();
    let id = alloc_server_id(&mut reg);
    reg.server_streams.insert(
        id,
        TlsServerStreamEntry {
            stream: TlsServerStream::Rustls(stream),
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
                .write_tls(&mut stream.sock)
                .map_err(|e| format!("handshake write: {}", e))?;
        }
        if stream.conn.wants_read() {
            let n = stream
                .conn
                .read_tls(&mut stream.sock)
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
    let debug_pls = std::env::var_os("CRATONVM_DBG_TLS_PLS").is_some();
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
    let client_identity = ctx_identity(ctx, ssl_context);
    // Server identity: same resolution `rustls_server_handshake_over_stream`'s
    // former caller used (this SSLContext's own identity, else the
    // process-wide runtime-configured one) — resolved here too so SERVER mode
    // never needs to touch `ssl_context` again.
    let server_identity = ctx_identity(ctx, ssl_context)
        .or_else(|| runtime_tls_identity().map(|identity| (identity.cert_pem, identity.key_pem)));
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
    if std::env::var_os("CRATONVM_DBG_TLS_PLS").is_some() {
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
        let provider = cipher_provider_for(&pending.enabled_ciphers);
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

pub(crate) fn rustls_stream_read(id: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    let debug_srv = std::env::var_os("CRATONVM_DBG_TLS_SRV").is_some();
    let mut reg = sreg().lock();
    if let Some(e) = reg.client_streams.get_mut(&id) {
        return e.stream.read(buf);
    }
    if let Some(e) = reg.server_streams.get_mut(&id) {
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] stream_read ENTER id={} requested_len={}",
                id,
                buf.len()
            );
        }
        let result = match &mut e.stream {
            TlsServerStream::Rustls(s) => s.read(buf),
            TlsServerStream::Native(s) => s.read(buf),
            #[cfg(unix)]
            TlsServerStream::LegacyDsa(s) => s.read(buf),
        };
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] stream_read RETURN id={} result={:?}",
                id, result
            );
        }
        return result;
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no such rustls stream id",
    ))
}

/// Write to either a client- or server-side rustls stream.
pub(crate) fn rustls_stream_write(id: i32, data: &[u8]) -> std::io::Result<usize> {
    let debug_srv = std::env::var_os("CRATONVM_DBG_TLS_SRV").is_some();
    let mut reg = sreg().lock();
    if let Some(e) = reg.client_streams.get_mut(&id) {
        return e.stream.write(data);
    }
    if let Some(e) = reg.server_streams.get_mut(&id) {
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] stream_write ENTER id={} len={}",
                id,
                data.len()
            );
        }
        let result = match &mut e.stream {
            TlsServerStream::Rustls(s) => s.write(data),
            TlsServerStream::Native(s) => s.write(data),
            #[cfg(unix)]
            TlsServerStream::LegacyDsa(s) => s.write(data),
        };
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] stream_write RETURN id={} result={:?}",
                id, result
            );
        }
        return result;
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
        match &mut e.stream {
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
        }
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
    let reg = sreg().lock();
    let entry = reg.client_streams.get(&id)?;
    let certs = entry.stream.conn.peer_certificates()?;
    if certs.is_empty() {
        return None;
    }
    Some(certs.iter().map(|c| c.as_ref().to_vec()).collect())
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

/// `SSLServerSocket` is a real JDK class, so its loaded instance layout is
/// not the compact synthetic layout expected by the TLS listener bridge.
/// Keep the authoritative lifecycle data outside the object: raw field writes
/// can be dropped or collide with reference-typed JDK fields, which otherwise
/// makes a newly-bound listener appear closed before its first accept.
#[derive(Clone, Copy)]
struct SslServerSocketState {
    listener_id: i32,
    local_port: i32,
    closed: i32,
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
        .copied()
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
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), ders.len());
            for (i, der) in ders.iter().enumerate() {
                let cert = alloc_concurrent_synthetic(ctx, "java/security/cert/X509Certificate", 4);
                // Best-effort CN extraction via the existing DER parser.
                let (subject, issuer) = crate::phases_late::basic_der_extract_names(der)
                    .unwrap_or_else(|| ("CN=Unknown".into(), "CN=Unknown".into()));
                let sub = ctx.create_string(&subject);
                let iss = ctx.create_string(&issuer);
                ctx.set_field(cert, 0, Value::Object(Some(sub)));
                ctx.set_field(cert, 1, Value::Object(Some(iss)));
                ctx.set_field(cert, 2, Value::Long(0));
                let der_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, der.len());
                for (j, &b) in der.iter().enumerate() {
                    ctx.set_array_element(der_arr, j, Value::Int(b as i8 as i32));
                }
                ctx.set_field(cert, 3, Value::Object(Some(der_arr)));
                ctx.set_array_element(arr, i, Value::Object(Some(cert)));
            }
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

fn create_ssl_server_socket(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    port: i32,
    bind_address: &str,
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
    let identity = args
        .first()
        .and_then(|value| match value {
            Value::Object(Some(factory)) if ctx.object_num_fields(*factory) > 0 => {
                match ctx.get_field(*factory, 0) {
                    Value::Object(Some(ssl_context)) => ctx_identity(ctx, ssl_context),
                    _ => None,
                }
            }
            _ => None,
        })
        .map(|(cert_pem, key_pem)| RuntimeTlsIdentity {
            cert_pem,
            key_pem,
            client_ca_pem: None,
        })
        .unwrap_or(require_runtime_tls_identity()?);
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
                format!("{rustls_error}; platform TLS fallback: {native_error}")
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

    let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocket", SSS_FIELDS);
    set_ssl_server_socket_state(
        ctx,
        obj,
        SslServerSocketState {
            listener_id: id,
            local_port: local_port as i32,
            closed: 0,
        },
    );
    ctx.set_field(obj, SSS_LISTENER_ID, Value::Int(id));
    ctx.set_field(obj, SSS_LOCAL_PORT, Value::Int(local_port as i32));
    ctx.set_field(obj, SSS_CLOSED, Value::Int(0));
    ctx.set_field(obj, 3, Value::Object(None));
    Ok(Some(Value::Object(Some(obj))))
}

fn ssl_server_bind_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    index: usize,
) -> Result<String, cratonvm_types::error::MethodCallFailed> {
    let address = obj_arg(args, index)?;
    let pin_base = ctx.pin_native_root(address);
    let resolved = ctx.invoke_virtual(address, "getHostAddress", "()Ljava/lang/String;", &[]);
    ctx.unpin_native_roots(pin_base);
    let host = match resolved? {
        Some(Value::Object(Some(value))) => ctx.read_string(value).unwrap_or_default(),
        _ => String::new(),
    };
    if host.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "InetAddress has no host address".into(),
        }
        .into());
    }
    Ok(host)
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
            create_ssl_server_socket(ctx, args, port, "0.0.0.0")
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
            create_ssl_server_socket(ctx, args, port, "0.0.0.0")
        },
    );
    r.register(
        sssf,
        "createServerSocket",
        "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
        |ctx, args| {
            let port = args.get(1).and_then(|value| value.as_int()).unwrap_or(0);
            let bind_address = ssl_server_bind_address(ctx, args, 3)?;
            create_ssl_server_socket(ctx, args, port, &bind_address)
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
        Ok(Some(Value::Int(
            ssl_server_socket_state(ctx, this)
                .map(|state| state.local_port)
                .unwrap_or_else(|| ctx.get_field(this, SSS_LOCAL_PORT).as_int().unwrap_or(0)),
        )))
    });
    r.register(sss, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            ssl_server_socket_state(ctx, this)
                .map(|state| state.closed)
                .unwrap_or_else(|| ctx.get_field(this, SSS_CLOSED).as_int().unwrap_or(1)),
        )))
    });
    r.register(sss, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = ssl_server_socket_state(ctx, this).unwrap_or(SslServerSocketState {
            listener_id: ctx.get_field(this, SSS_LISTENER_ID).as_int().unwrap_or(-1),
            local_port: ctx.get_field(this, SSS_LOCAL_PORT).as_int().unwrap_or(0),
            closed: 1,
        });
        let id = state.listener_id;
        if id >= 0 {
            rustls_listener_close(id);
            ctx.set_field(this, SSS_LISTENER_ID, Value::Int(-1));
        }
        set_ssl_server_socket_state(
            ctx,
            this,
            SslServerSocketState {
                listener_id: -1,
                local_port: state.local_port,
                closed: 1,
            },
        );
        ctx.set_field(this, SSS_CLOSED, Value::Int(1));
        Ok(None)
    });
    r.register(sss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ssl_server_socket_state(ctx, this)
            .map(|state| state.listener_id)
            .unwrap_or_else(|| ctx.get_field(this, SSS_LISTENER_ID).as_int().unwrap_or(-1));
        if id < 0 {
            return Err(RuntimeError::IOException {
                message: "SSLServerSocket is closed".into(),
            }
            .into());
        }
        let stream_id =
            rustls_server_accept(id).map_err(|e| RuntimeError::IOException { message: e })?;

        // Build an SSLSocket wrapper. Reuses the existing SSLSocket/
        // SSLSocketInputStream/SSLSocketOutputStream classes but puts
        // the rustls stream id into field 2. The stream I/O natives
        // dispatch on stream-id-table membership (rustls tables first,
        // then fall back to native-tls).
        let sock = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", SSS_SOCK_FIELDS);
        let (proto, cipher, alpn, sni) = rustls_session_info(stream_id)
            .unwrap_or_else(|| ("TLSv1.3".into(), "UNKNOWN".into(), None, None));
        let host_str = ctx.create_string(sni.as_deref().unwrap_or("server"));
        ctx.set_field(sock, SSS_SOCK_HOST, Value::Object(Some(host_str)));
        ctx.set_field(sock, SSS_SOCK_PORT, Value::Int(0));
        ctx.set_field(sock, SSS_SOCK_TLSID, Value::Int(stream_id));
        ctx.set_field(sock, SSS_SOCK_CLOSED, Value::Int(0));
        // NOTE: a blocking read/write on this accepted socket appears to be
        // unreliable independent of this doc's fix (probed while validating
        // the accept-path change below; H2's own TestNetUtils never reads or
        // writes on the accepted socket, so it's outside this doc's scope —
        // left uninvestigated rather than risk a half-understood change to
        // this shared accept path).

        let session = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 3);
        let p = ctx.create_string(&proto);
        let c = ctx.create_string(&cipher);
        ctx.set_field(session, 0, Value::Object(Some(p)));
        ctx.set_field(session, 1, Value::Object(Some(c)));
        ctx.set_field(session, 2, Value::Int(stream_id));
        ctx.set_field(sock, SSS_SOCK_SESSION, Value::Object(Some(session)));
        // Stash ALPN on the socket so `getApplicationProtocol()` can read it.
        // We use a side-table rather than widening SSLSocket's shape.
        if let Some(alpn_str) = alpn {
            stash_sock_alpn(ctx, sock, alpn_str);
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
            if list.is_empty() {
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
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), list.len());
            for (i, p) in list.iter().enumerate() {
                let s = ctx.create_string(p);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        sss,
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
    r.set_category(__prev_cat);
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
/// `docs/internal/fixed-suite-bugs/reactive-httpcomponents-connector-flaky-tls-engine-identity-and-pool-cipher-leak-FIXED.md`)
/// — that earlier fix's scope note explicitly left
/// `ssl_server_socket_states`, `sock_alpn_table`, `session_peer_certs_table`,
/// and `SSLSession.getId()`'s seed unfixed; this closes those.
/// `ctx.identity_hash_code` is the VM's real, GC-stable identity hash,
/// computed once and pinned for an object's lifetime regardless of later
/// moves.
fn gc_stable_objref_key(ctx: &dyn NativeContext, o: ObjectRef) -> u64 {
    ctx.identity_hash_code(o) as u32 as u64
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

    r.register(
        hurl,
        "getDefaultSSLSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // Capture the client identity (cert+key) carried by the factory's
    // SSLContext so the native HttpsURLConnection client can present a client
    // certificate for mTLS. `setDefaultSSLSocketFactory` is static (factory =
    // args[0]); `setSSLSocketFactory` is instance (factory = args[1]).
    fn capture_huc_client_identity(
        ctx: &mut dyn cratonvm_native_api::NativeContext,
        factory: ObjectRef,
        connection: Option<ObjectRef>,
    ) {
        if let Value::Object(Some(sslctx)) = ctx.get_field(factory, 0) {
            if let Some(connection) = connection {
                capture_huc_ssl_context_for_connection(ctx, connection, sslctx);
            } else {
                capture_huc_ssl_context(ctx, sslctx);
            }
        }
    }
    r.register(
        hurl,
        "setDefaultSSLSocketFactory",
        "(Ljavax/net/ssl/SSLSocketFactory;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(f))) = args.first() {
                capture_huc_client_identity(ctx, *f, None);
            }
            Ok(None)
        },
    );
    r.register(
        hurl,
        "setSSLSocketFactory",
        "(Ljavax/net/ssl/SSLSocketFactory;)V",
        |ctx, args| {
            if let (Some(Value::Object(Some(connection))), Some(Value::Object(Some(f)))) =
                (args.first(), args.get(1))
            {
                capture_huc_client_identity(ctx, *f, Some(*connection));
            }
            Ok(None)
        },
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
    // HostnameVerifier.verify — this native is registered on the *interface*
    // `javax/net/ssl/HostnameVerifier`, so it can be reached two ways:
    //
    //   1. The VM's OWN default verifier, allocated above via
    //      `alloc_concurrent_synthetic(ctx, "javax/net/ssl/HostnameVerifier", 0)`.
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
    use cratonvm_native_api::NativeContext;
    use cratonvm_types::ObjectRef;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

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
        let bb = alloc_concurrent_synthetic(&mut ctx, "java/nio/HeapByteBuffer", 8);
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
        let bb = alloc_concurrent_synthetic(&mut ctx, "java/nio/DirectByteBuffer", 8);
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
        let bb = alloc_concurrent_synthetic(&mut ctx, "java/nio/DirectByteBuffer", 8);
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
        let bb = alloc_concurrent_synthetic(&mut ctx, "javax/net/ssl/SyntheticBuf", 4);
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

    /// A shape we cannot resolve must move zero bytes (and not panic).
    #[test]
    fn bb_view_unresolved_moves_zero_bytes() {
        let mut ctx = crate::test_utils::mock_ctx();
        let bb = alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 3);
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
        attach_pending_identity_to_ctx(&mut mock_ctx, ctx, None);

        assert!(ctx_identity(&mut mock_ctx, ctx).is_none());
        let selected = selected_context_trust_roots().expect("context trust roots selected");
        assert_eq!(selected.root_ders, vec![ca_der]);
        let root_store = root_store_for_trust_roots(Some(&selected));
        assert_eq!(root_store.roots.len(), 1);

        let other_ctx = fake_object_ref(2);
        assert!(ctx_identity(&mut mock_ctx, other_ctx).is_none());
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
                        .read_tls(&mut stream.sock)
                        .map_err(|e| e.to_string())?;
                    stream
                        .conn
                        .process_new_packets()
                        .map_err(|e| e.to_string())?;
                }
                if stream.conn.wants_write() {
                    stream
                        .conn
                        .write_tls(&mut stream.sock)
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
                        stream
                            .conn
                            .read_tls(&mut stream.sock)
                            .map_err(|e| e.to_string())?;
                        stream
                            .conn
                            .process_new_packets()
                            .map_err(|e| e.to_string())?;
                    }
                    if stream.conn.wants_write() {
                        stream
                            .conn
                            .write_tls(&mut stream.sock)
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
                            .read_tls(&mut stream.sock)
                            .map_err(|e| e.to_string())?;
                        stream
                            .conn
                            .process_new_packets()
                            .map_err(|e| e.to_string())?;
                    }
                    if stream.conn.wants_write() {
                        stream
                            .conn
                            .write_tls(&mut stream.sock)
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
                    stream.conn.write_tls(&mut stream.sock).unwrap();
                }
                if stream.conn.wants_read() {
                    stream.conn.read_tls(&mut stream.sock).unwrap();
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
                    stream.conn.write_tls(&mut stream.sock).unwrap();
                }
                if stream.conn.wants_read() {
                    stream.conn.read_tls(&mut stream.sock).unwrap();
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
            identity_override: None,
            trust_roots_override: None,
            plaintext_pending: Vec::new(),
            peer_cert_chain_der: Vec::new(),
            trust_managers_ctx_key: None,
            trust_check_done: false,
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
            if std::env::var_os("CRATONVM_DBG_TLS_HS").is_some() {
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
) -> ObjectRef {
    // Build a REAL SSLEngineResult via its public ctor with REAL enum constants,
    // so `getStatus()`/`getHandshakeStatus()` return singletons the connector
    // can `==`-compare. (The old synthetic int-slot object made every enum
    // comparison fail → the NIO handshake state machine spun → native SO.)
    let st = real_status_enum(ctx, status);
    let hss = real_handshake_status_enum(ctx, hs);
    let __dbg_hs = std::env::var_os("CRATONVM_DBG_TLS_HS").is_some();
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
                return o;
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
    // `write_tls()` may have produced more than one complete TLS record. A
    // previous wrap can legitimately emit only the first record when the
    // caller's destination buffer is smaller than the whole flight; keep
    // driving wrap until that queued remainder is on the wire.
    if !s.outbound.is_empty() {
        return HS_NEED_WRAP_R;
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
/// `docs/known-issues/reactive-netty-https-sslengine-handshake-underflow.md`.
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
        BbBacking::Heap { arr, off } => (from..to)
            .map(|i| ctx.get_array_element(arr, off + i).as_int().unwrap_or(0) as u8)
            .collect(),
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
        BbBacking::Heap { arr, off } => {
            for (k, b) in data.iter().enumerate() {
                ctx.set_array_element(arr, off + at + k, Value::Int(*b as i8 as i32));
            }
            data.len()
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

/// Begin the handshake — construct the rustls connection from the cached
/// configs (or defaults) and stash it on the engine.
fn engine_begin(state: &mut EngineState) -> Result<(), String> {
    if state.conn.is_some() {
        if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
            eprintln!(
                "[dbg-tls-auth] engine_begin SHORT-CIRCUIT (conn already realized) need={} want={}",
                state.need_client_auth, state.want_client_auth
            );
        }
        return Ok(());
    }
    let alpn_strs: Vec<&str> = state
        .alpn_protocols
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .collect();
    if state.is_client {
        let config = match state.client_config.clone() {
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
                let provider = cipher_provider_for(&state.enabled_ciphers);
                build_client_config_ex_with_provider(
                    roots,
                    &alpn_strs,
                    ClientAuthMode::Fixed(client_auth),
                    revocation,
                    use_java_trust_manager,
                    provider,
                )?
            }
        };
        let host = state
            .peer_host
            .clone()
            .unwrap_or_else(|| "localhost".to_string());
        let server_name =
            ServerName::try_from(host).map_err(|e| format!("invalid SNI hostname: {}", e))?;
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
                    // `docs/internal/fixed-suite-bugs/tls-ocsp-clientcert-
                    // validation-not-enforced-FIXED.md`, "Residual #2 implementation"
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
                    let speculative_optional_auth = false
                        && !state.need_client_auth
                        && !state.want_client_auth
                        && state
                            .trust_roots_override
                            .as_ref()
                            .map(|r| !r.root_ders.is_empty())
                            .unwrap_or(false);
                    let request = state.need_client_auth
                        || state.want_client_auth
                        || speculative_optional_auth;
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
                    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
                        eprintln!(
                            "[dbg-tls-auth] engine_begin request={} client_ca_none={} trust_ctx_key={:?} has_custom_trust_managers={}",
                            request, client_ca.is_none(), state.trust_managers_ctx_key, has_custom_trust_managers
                        );
                    }
                    let built = if has_custom_trust_managers {
                        build_server_config_single_cert_passthrough_client_auth(
                            cert,
                            key,
                            &alpn_strs,
                            state.need_client_auth,
                            &state.enabled_ciphers,
                        )
                    } else {
                        build_server_config_single_cert_ex_ciphers(
                            cert,
                            key,
                            &alpn_strs,
                            state.need_client_auth,
                            (state.want_client_auth || speculative_optional_auth)
                                && !state.need_client_auth,
                            client_ca.as_deref(),
                            &state.enabled_ciphers,
                        )
                    };
                    built?
                }
                None => {
                    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
                        eprintln!(
                            "[dbg-tls-auth] engine_begin(default_engine_server_config) need={} want={}",
                            state.need_client_auth, state.want_client_auth
                        );
                    }
                    default_engine_server_config(&state.alpn_protocols, state.need_client_auth)?
                }
            },
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
    is_client: bool,
    peer_chain_der: Vec<Vec<u8>>,
    trust_ctx_key: u64,
    negotiated_cipher_suite_name: Option<String>,
}

/// Called right after the crypto handshake reports FINISHED for the first
/// time, WHILE STILL HOLDING the engine registry lock — extracts what's
/// needed and returns `Some` at most once per engine (gated by
/// `trust_check_done`). The caller MUST drop the registry lock before acting
/// on the result: looking up `ctx_trust_managers_table` and invoking Java is
/// deferred to `engine_run_trust_check` specifically so no allocating/GC-
/// triggering call ever happens while this lock is held (see
/// `EngineState::trust_managers_ctx_key`'s doc for why that matters).
fn engine_take_pending_trust_check(state: &mut EngineState) -> Option<PendingTrustCheck> {
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
    let trust_ctx_key = state.trust_managers_ctx_key?;
    if state.peer_cert_chain_der.is_empty() {
        // No peer certificate was presented (e.g. optional client auth and
        // the client declined) — nothing for a TrustManager to check.
        return None;
    }
    let cipher_name = state
        .conn
        .as_ref()
        .and_then(|c| c.negotiated_cipher_suite())
        .map(|cs| format!("{:?}", cs.suite()));
    Some(PendingTrustCheck {
        is_client: state.is_client,
        peer_chain_der: state.peer_cert_chain_der.clone(),
        trust_ctx_key,
        negotiated_cipher_suite_name: cipher_name,
    })
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
fn engine_run_trust_check(
    ctx: &mut dyn NativeContext,
    pending: PendingTrustCheck,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let trust_managers = ctx_trust_managers_table()
        .lock()
        .get(&pending.trust_ctx_key)
        .cloned()
        .unwrap_or_default();
    if trust_managers.is_empty() {
        // No custom TrustManager/TrustManagerFactory installed on this
        // context — rustls's own chain-of-trust check is the only
        // verification, matching prior (pre-fix) behavior exactly.
        return Ok(());
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
    for (i, der) in pending.peer_chain_der.iter().enumerate() {
        let mirror = crate::keystore::make_x509_mirror(ctx, "peer", der);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    let auth_type_str = ctx.create_string(auth_type);

    // Pin the chain array, the authType string, and every TrustManager we're
    // about to call — each `invoke_virtual` below can allocate/GC, and a
    // stale ObjectRef from an earlier loop iteration would silently resolve
    // to a reused slot after a move (see `pin_native_root`'s doc). Mirrors
    // the existing multi-call pin pattern in `net_phase_e.rs`'s
    // group-collector native.
    let base = ctx.pin_native_root(arr);
    let _ = ctx.pin_native_root(auth_type_str);
    let tm_pins: Vec<usize> = trust_managers
        .iter()
        .map(|tm| ctx.pin_native_root(*tm))
        .collect();

    let method = if pending.is_client {
        "checkServerTrusted"
    } else {
        "checkClientTrusted"
    };
    let dbg = std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok();
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
    for (i, _tm) in trust_managers.iter().enumerate() {
        let arr_now = ctx.read_native_pin(base, arr);
        let auth_now = ctx.read_native_pin(base + 1, auth_type_str);
        let tm_now = ctx.read_native_pin(tm_pins[i], trust_managers[i]);
        let result = ctx.invoke_virtual(
            tm_now,
            method,
            "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V",
            &[Value::Object(Some(arr_now)), Value::Object(Some(auth_now))],
        );
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
        if result.is_err() {
            rejected = true;
            break;
        }
    }
    ctx.unpin_native_roots(base);

    if rejected {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/net/ssl/SSLHandshakeException",
            "TrustManager rejected the peer certificate chain",
        ));
    }
    Ok(())
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
            is_client: true,
            peer_chain_der,
            trust_ctx_key,
            negotiated_cipher_suite_name: None,
        },
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
/// Shared by `getSession()` and `getHandshakeSession()` — see the latter's
/// registration for why real JDK's `getHandshakeSession()` cannot be left
/// un-intercepted on this engine implementation.
fn build_synthetic_ssl_session(ctx: &mut dyn NativeContext, id: i32) -> ObjectRef {
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
    .unwrap_or_else(|| {
        (
            "TLSv1.3".into(),
            "TLS_AES_256_GCM_SHA384".into(),
            String::new(),
        )
    });
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
    // Associate the peer (client) cert chain with this session object so
    // SSLSession.getPeerCertificates() can return it for mTLS auth.
    let peer_chain = with_engine(id, |s| s.peer_cert_chain_der.clone()).unwrap_or_default();
    if !peer_chain.is_empty() {
        session_peer_certs_table()
            .lock()
            .insert(gc_stable_objref_key(ctx, ses), peer_chain);
    }
    ses
}

fn register_engine_impl_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cls_impl = "sun/security/ssl/SSLEngineImpl";

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

    // Netty configures JDK ALPN support through this concrete implementation
    // method. A rustls-backed engine is deliberately allocated without
    // SunJSSE's private `conContext` graph, so interpreting the real body
    // dereferences that absent state before the native handshake starts. The
    // callback is only used by SunJSSE's own ALPN selector; rustls performs
    // the negotiated-protocol selection itself from `SSLParameters`.
    r.register(
        cls_impl,
        "setHandshakeApplicationProtocolSelector",
        "(Ljava/util/function/BiFunction;)V",
        |_ctx, _args| Ok(None),
    );
    // Netty probes the paired getter when deciding whether JDK ALPN support is
    // active.  It has the same `conContext` dependency as the setter above;
    // rustls owns ALPN negotiation, so no Java-side selector is installed.
    r.register(
        cls_impl,
        "getHandshakeApplicationProtocolSelector",
        "()Ljava/util/function/BiFunction;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    r.register(cls_impl, "setNeedClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|x| x.as_int()).unwrap_or(0) != 0;
        let id = engine_id_or_alloc(ctx, this);
        if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
            let id = engine_id_or_alloc(ctx, this);
            let list = with_engine(id, |s| s.enabled_protocols.clone())
                .unwrap_or_else(|| vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()]);
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

    // getSupportedCipherSuites() — the connector validates its configured cipher
    // list against this; without it the engine reported zero supported ciphers
    // and Tomcat threw "None of the [ciphers] specified are supported by the SSL
    // engine". Return the rustls-negotiable suites (overlaps Tomcat's defaults).
    r.register(
        cls_impl,
        "getSupportedCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let suites = [
                "TLS_AES_128_GCM_SHA256",
                "TLS_AES_256_GCM_SHA384",
                "TLS_CHACHA20_POLY1305_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
                "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
                "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
                "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
                "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
                // T-CBC.1: real (not just reported) CBC-mode suites, see
                // `t27_tls_cbc` / docs/known-issues/springboot/rustls-cbc-cipher-suites-not-supported.md
                "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256",
                "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384",
                "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384",
            ];
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
    r.register(cls_impl, "beginHandshake", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
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
        if std::env::var("CRATONVM_DBG_TLS_HS").is_ok() {
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

    r.register(cls_impl, "closeInbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        with_engine(id, |s| {
            s.closed_inbound = true;
        });
        Ok(None)
    });

    r.register(cls_impl, "isInboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        let v = with_engine(id, |s| s.closed_inbound).unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    });

    r.register(cls_impl, "isOutboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
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
            let id = engine_id_or_alloc(ctx, this);
            Ok(Some(Value::Object(Some(build_synthetic_ssl_session(
                ctx, id,
            )))))
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
    // docs/known-issues/springboot/http-client-connector-teardown-hang-crash.md.
    // Real JDK's `getHandshakeSession()` returns the session being
    // negotiated (or null outside a handshake); returning the same
    // best-effort synthetic session `getSession()` already builds (complete
    // with graceful "no negotiation yet" defaults) is sufficient for every
    // caller in this codebase's suites, which only use it for buffer sizing.
    r.register(
        cls_impl,
        "getHandshakeSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            Ok(Some(Value::Object(Some(build_synthetic_ssl_session(
                ctx, id,
            )))))
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
    let id = engine_id_or_alloc(ctx, this);
    let __dbg_hs = std::env::var("CRATONVM_DBG_TLS_HS").is_ok();
    if __dbg_hs {
        eprintln!(
            "[dbg-tls-hs] thread={:?} do_wrap ENTER id={}",
            std::thread::current().id(),
            id
        );
    }

    // Closed-outbound short-circuit.
    let closed = with_engine(id, |s| s.closed_outbound).unwrap_or(false);
    if closed {
        if __dbg_hs {
            eprintln!(
                "[dbg-tls-hs] thread={:?} do_wrap id={} CLOSED_OUTBOUND_SHORT_CIRCUIT",
                std::thread::current().id(),
                id
            );
        }
        let result = alloc_engine_result(ctx, SR_CLOSED, HS_NOT_HANDSHAKING_R, 0, 0);
        return Ok(Some(Value::Object(Some(result))));
    }

    // Lazily realize rustls connection.
    {
        let mut g = engine_registry().write();
        if let Some(s) = g.get_mut(&id) {
            if s.conn.is_none() {
                if let Err(e) = engine_begin(s) {
                    if std::env::var("CRATONVM_DBG_TLS_HS").is_ok() {
                        eprintln!(
                            "[dbg-tls-hs] thread={:?} do_unwrap/do_wrap id={} RETURN(engine_begin ERROR) err={}",
                            std::thread::current().id(), id, e
                        );
                    }
                    return Err(RuntimeError::IOException { message: e }.into());
                }
            }
        }
    }

    // Step 1: read app data from src ByteBuffers (only relevant when not handshaking).
    let mut app_bytes = Vec::new();
    let mut consumed_app = 0usize;
    let needs_app_data = with_engine(id, |s| {
        s.conn
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

    let (consumed_inner, status, hs, drained, pending_trust_check) = {
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
        // Drain only complete TLS records. A partial record written to the
        // channel cannot be recovered by a later wrap call.
        let take = complete_tls_record_prefix(&s.outbound, dst_remaining);
        let drained: Vec<u8> = s.outbound.drain(0..take).collect();

        // Report overflow only when the destination cannot hold even the next
        // complete record. If at least one record was emitted, returning OK
        // lets Tomcat flush it and call wrap again for the remaining record.
        let status = if drained.is_empty() && !s.outbound.is_empty() {
            SR_BUFFER_OVERFLOW
        } else {
            SR_OK
        };
        engine_capture_negotiation(s);
        let hs = handshake_status_of(s);
        if hs == HS_FINISHED_R {
            s.handshake_finished_reported = true;
        }
        // Extract-only — see `engine_take_pending_trust_check`'s doc for why
        // the actual Java call must happen after this lock is dropped.
        let pending_trust_check = engine_take_pending_trust_check(s);
        (cons, status, hs, drained, pending_trust_check)
    };
    if let Some(pending) = pending_trust_check {
        engine_run_trust_check(ctx, pending)?;
    }

    // Step 3: write the drained bytes into dst.
    let produced = if !drained.is_empty() {
        bb_write_from(ctx, dst, &drained)
    } else {
        0
    };

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
    let id = engine_id_or_alloc(ctx, this);
    let __dbg_hs = std::env::var("CRATONVM_DBG_TLS_HS").is_ok();
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
        return Ok(Some(Value::Object(Some(result))));
    }

    {
        let mut g = engine_registry().write();
        if let Some(s) = g.get_mut(&id) {
            if s.conn.is_none() {
                if let Err(e) = engine_begin(s) {
                    if std::env::var("CRATONVM_DBG_TLS_HS").is_ok() {
                        eprintln!(
                            "[dbg-tls-hs] thread={:?} do_unwrap/do_wrap id={} RETURN(engine_begin ERROR) err={}",
                            std::thread::current().id(), id, e
                        );
                    }
                    return Err(RuntimeError::IOException { message: e }.into());
                }
            }
        }
    }

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
            let n = bb_write_from(ctx, *d, &pending[idx..]);
            idx += n;
            if n == 0 {
                break;
            }
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
        return Ok(Some(Value::Object(Some(result))));
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
        return Ok(Some(Value::Object(Some(result))));
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

    let (status, hs, plaintext, pending_trust_check) = {
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
        let mut plaintext: Vec<u8> = Vec::new();
        let mut underflow = false;
        let src_resolved = !matches!(src_view.backing, BbBacking::Unresolved);
        if let (true, Some(conn)) = (src_resolved, s.conn.as_mut()) {
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
                // Don't start a record whose plaintext would overflow the dst
                // (once we already have some to deliver). `rec_len >= plaintext`
                // is a safe upper bound (TLS overhead only shrinks it).
                if !plaintext.is_empty() && plaintext.len() + rec_len > dst_cap {
                    break;
                }
                let rec_total = 5 + rec_len;
                let rec = bb_bytes_range(ctx, &src_view, offset, rec_end);
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
                if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
                        if matches!(&*conn, EngineConn::Server(_)) {
                            offset = rec_end;
                            break;
                        }
                        return Err(crate::phases_early::throw_jca_exc(
                            ctx,
                            "javax/net/ssl/SSLHandshakeException",
                            &format!("rustls: {}", e),
                        ));
                    }
                    return Err(RuntimeError::IOException {
                        message: format!("rustls process_new_packets: {}", e),
                    }
                    .into());
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
        engine_capture_negotiation(s);
        let _ = underflow;
        let mut status = SR_OK;
        if plaintext.is_empty() && offset == src_pos {
            // No progress: src was empty or held only an incomplete record. Tell
            // the caller to read more network data (matches SSLEngine semantics;
            // returning OK here makes Tomcat's handshake loop spin forever).
            status = SR_BUFFER_UNDERFLOW;
        }
        let hs = handshake_status_of(s);
        if hs == HS_FINISHED_R {
            s.handshake_finished_reported = true;
        }
        // Extract-only — see `engine_take_pending_trust_check`'s doc for why
        // the actual Java call must happen after this lock is dropped.
        let pending_trust_check = engine_take_pending_trust_check(s);
        (status, hs, plaintext, pending_trust_check)
    };
    if let Some(pending) = pending_trust_check {
        engine_run_trust_check(ctx, pending)?;
    }
    let consumed = offset - src_pos;
    bb_set_pos(ctx, src, src_view.layout, offset);

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
    Ok(Some(Value::Object(Some(result))))
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
                .unwrap_or_else(|| vec!["h2".into(), "http/1.1".into()]);
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), list.len());
            for (i, p) in list.iter().enumerate() {
                let s = ctx.create_string(p);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(arr))))
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
                if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
            const DEFAULT_CIPHERS: &[&str] = &[
                "TLS_AES_128_GCM_SHA256",
                "TLS_AES_256_GCM_SHA384",
                "TLS_CHACHA20_POLY1305_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
                "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
                "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
                "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
                "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
                // T-CBC.1: real (not just reported) CBC-mode suites, see
                // `t27_tls_cbc` / docs/known-issues/springboot/rustls-cbc-cipher-suites-not-supported.md
                "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256",
                "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384",
                "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384",
            ];
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
                _ => alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLParameters", 4),
            };
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
) {
    let key = ctx_obj_key(ctx, ctx_obj);
    let id = engine_id_or_alloc(ctx, engine_obj);
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
        let has_entry = ctx_trust_managers_table().lock().contains_key(&key);
        eprintln!(
            "[dbg-tls-auth] set_engine_trust_ctx_key engine_id={} ctx_key={} table_has_entry={}",
            id, key, has_entry
        );
    }
    with_engine(id, |s| {
        s.trust_managers_ctx_key = Some(key);
    });
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
}

/// Post-move remap companion to `gc_scan_tls_ctx_trust_manager_roots`.
pub fn gc_update_tls_ctx_trust_manager_refs(map: &std::collections::HashMap<usize, usize>) {
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
pub fn gc_update_tls_ctx_key_manager_refs(map: &std::collections::HashMap<usize, usize>) {
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
    if std::env::var("CRATONVM_DBG_TLS_AUTH").is_ok() {
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
pub fn gc_update_default_ssl_context_ref(map: &std::collections::HashMap<usize, usize>) {
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
/// session objects the VM hands out — the 7-field session from
/// `SSLEngineImpl.getSession()` (see ~line 3263) and the 3-field session from
/// `SSLServerSocket.accept()` (see ~line 1014). Both objects carry the bare
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
fn session_peer_certs_table() -> &'static Mutex<HashMap<u64, Vec<Vec<u8>>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<Vec<u8>>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
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

fn register_ssl_session_real(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLSession";

    // getPeerCertificates() — the client certificate chain, for mTLS. Tomcat's
    // SSLAuthenticator / coyote SSLSupport reads this to authenticate the
    // client; without it a client-cert-protected resource returns HTTP 401.
    // Build real `sun.security.x509.X509CertImpl` mirrors from the captured DER
    // (same path the keystore uses). Empty chain → throw
    // SSLPeerUnverifiedException (real-JDK contract), which Tomcat treats as
    // "no client cert".
    r.register(
        cls,
        "getPeerCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let chain = session_peer_certs_table()
                .lock()
                .get(&gc_stable_objref_key(ctx, this))
                .cloned()
                .unwrap_or_default();
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
                let mirror = crate::keystore::make_x509_mirror(ctx, "peer", der);
                ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // Buffer sizes are layout-independent JSSE constants. The real JDK returns
    // 16384 (max TLS plaintext record) for `getApplicationBufferSize` and 16709
    // (16384 + TLS record overhead: 5 header + 256 padding + 68 MAC/IV) for
    // `getPacketBufferSize`. Tomcat only needs them >= a TLS record so its
    // network/application `ByteBuffer`s are large enough.
    r.register(cls, "getApplicationBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(16384)))
    });
    r.register(cls, "getPacketBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(16709)))
    });
    // getId() — Tomcat's request/auth plumbing reads the TLS session id (e.g.
    // for SSL session tracking / client-cert requests). Real JDK returns the
    // negotiated session id bytes; the abstract interface declaration has no
    // Code, so without a real-mode native this throws AbstractMethodError and
    // every HTTPS request to a protected resource fails (HTTP -1). Return a
    // stable 32-byte id derived from the session object's identity.
    r.register(cls, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
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

    // proto/cipher slot order differs between the two real-mode shapes:
    //   7-field (SSLEngineImpl.getSession): [0]=cipher [1]=proto [2]=valid ...
    //   3-field (SSLServerSocket.accept):   [0]=proto  [1]=cipher [2]=stream_id
    // Disambiguate by field count so both return the correct String.
    r.register(cls, "getProtocol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let slot = if ctx.object_num_fields(this) >= 7 {
            1
        } else {
            0
        };
        Ok(Some(ctx.get_field(this, slot)))
    });
    r.register(
        cls,
        "getCipherSuite",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let slot = if ctx.object_num_fields(this) >= 7 {
                0
            } else {
                1
            };
            Ok(Some(ctx.get_field(this, slot)))
        },
    );

    // `isValid` flag is slot 2 only on the 7-field engine session; the 3-field
    // accept session has no flag — treat it as valid (it was just negotiated).
    r.register(cls, "isValid", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) >= 7 {
            Ok(Some(ctx.get_field(this, 2)))
        } else {
            Ok(Some(Value::Int(1)))
        }
    });

    // creation / last-accessed time: the 7-field engine session stores a
    // millis timestamp in slot 5; the 3-field shape has none → 0.
    r.register(cls, "getCreationTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 5 {
            Ok(Some(ctx.get_field(this, 5)))
        } else {
            Ok(Some(Value::Long(0)))
        }
    });
    r.register(cls, "getLastAccessedTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 5 {
            Ok(Some(ctx.get_field(this, 5)))
        } else {
            Ok(Some(Value::Long(0)))
        }
    });
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
