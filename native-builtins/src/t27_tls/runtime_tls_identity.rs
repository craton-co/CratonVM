#[allow(unused_imports)]
use super::*;


thread_local! {
    // Re-entrancy guard for the interface-level `HostnameVerifier.verify`
    // native. When we re-dispatch a custom verifier's `verify()` via
    // `ctx.invoke`, the VM's interface resolution could (in the degenerate
    // case of a subclass that defines no bytecode override) route back into
    // this same native. The guard breaks that loop: a re-entrant call falls
    // back to the default "accept — rustls already validated SNI" behavior
    // instead of recursing forever.
    pub static HOSTNAME_VERIFY_REENTRANT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
pub struct RuntimeTlsIdentity {
    pub cert_pem: String,
    pub key_pem: String,
    /// Optional client-CA bundle used when the server is asked to perform
    /// mTLS via `setNeedClientAuth(true)`. `None` means client auth is
    /// rejected with a config error rather than silently disabled.
    pub client_ca_pem: Option<String>,
}

/// One slot PER VM (gc-common w11-a,
/// `common-w10e-runtime-tls-identity-is-one-per-process`). It was one
/// `OnceLock<Mutex<Option<..>>>` for the whole process, so a keystore VM A
/// loaded (a certificate chain AND its private key) was served by VM B's
/// `SSLServerSocket` or server `SSLEngine` whenever B's context carried no
/// identity of its own -- and whichever VM loaded a usable key last won.
/// Dropped by `forget_vm_tls_state`.
pub static RUNTIME_TLS_IDENTITY: cratonvm_native_api::vm_scoped::VmScoped<Option<RuntimeTlsIdentity>> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Public setter for VM bootstrap / keystore-configuration code. Replaces any
/// identity VM `vm` previously installed. Pass `None` to clear (used by tests
/// to restore a known starting state).
#[allow(dead_code)]
pub fn set_runtime_tls_identity(vm: usize, identity: Option<RuntimeTlsIdentity>) {
    match identity {
        Some(identity) => RUNTIME_TLS_IDENTITY.with(vm, |slot| *slot = Some(identity)),
        // Clearing never creates a row.
        None => {
            if RUNTIME_TLS_IDENTITY.has_row(vm) {
                RUNTIME_TLS_IDENTITY.with(vm, |slot| *slot = None);
            }
        }
    }
}

/// Fetch the runtime TLS identity VM `vm` configured (if any).
pub fn runtime_tls_identity(vm: usize) -> Option<RuntimeTlsIdentity> {
    RUNTIME_TLS_IDENTITY.peek(vm, |slot| slot.clone()).flatten()
}

/// Return VM `vm`'s runtime TLS identity or the canonical
/// `IllegalStateException` that every server-side entry point uses when no
/// keystore is configured.
pub fn require_runtime_tls_identity(vm: usize) -> Result<RuntimeTlsIdentity, RuntimeError> {
    runtime_tls_identity(vm).ok_or_else(|| RuntimeError::IllegalStateException {
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
    pub static PENDING_KM_IDENTITY: std::cell::RefCell<Option<(String, String)>> =
        const { std::cell::RefCell::new(None) };
    pub static PENDING_TM_TRUST_ROOTS: std::cell::RefCell<Option<TlsTrustRoots>> =
        const { std::cell::RefCell::new(None) };
    pub static SELECTED_CONTEXT_TRUST_ROOTS: std::cell::RefCell<Option<TlsTrustRoots>> =
        const { std::cell::RefCell::new(None) };
}

/// `KeyManagerFactory.init` calls this with the keystore's PEM identity.
pub fn set_pending_km_identity(cert_pem: String, key_pem: String) {
    PENDING_KM_IDENTITY.with(|c| *c.borrow_mut() = Some((cert_pem, key_pem)));
}

pub fn take_pending_km_identity() -> Option<(String, String)> {
    PENDING_KM_IDENTITY.with(|c| c.borrow_mut().take())
}

#[derive(Clone, Debug, Default)]
pub struct TlsTrustRoots {
    pub root_ders: Vec<Vec<u8>>,
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
    pub revocation: Option<crate::x509_manager::RevocationConfig>,
}

pub fn dedup_root_ders(root_ders: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut deduped: Vec<Vec<u8>> = Vec::new();
    for der in root_ders {
        if !der.is_empty() && !deduped.iter().any(|r| r == &der) {
            deduped.push(der);
        }
    }
    deduped
}

/// `TrustManagerFactory.init` calls this with the exact anchor set that should
/// be scoped to the next `SSLContext.init` on this thread. A configured custom
/// truststore is therefore restrictive instead of being added to native roots.
pub fn set_pending_tm_trust_roots(root_ders: Vec<Vec<u8>>) {
    let deduped = dedup_root_ders(root_ders);
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
pub fn set_pending_tm_revocation(revocation: Option<crate::x509_manager::RevocationConfig>) {
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

pub fn take_pending_tm_trust_roots() -> Option<TlsTrustRoots> {
    let out = PENDING_TM_TRUST_ROOTS.with(|c| c.borrow_mut().take());
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] take_pending_tm_trust_roots -> {:?}",
            out.as_ref().map(|r| r.root_ders.len())
        );
    }
    out
}

pub fn set_selected_context_trust_roots(roots: Option<TlsTrustRoots>) {
    SELECTED_CONTEXT_TRUST_ROOTS.with(|c| *c.borrow_mut() = roots);
}

pub fn selected_context_trust_roots() -> Option<TlsTrustRoots> {
    SELECTED_CONTEXT_TRUST_ROOTS.with(|c| c.borrow().clone())
}

#[cfg(unix)]
pub fn selected_context_trust_root_ders() -> Vec<Vec<u8>> {
    selected_context_trust_roots()
        .map(|roots| roots.root_ders)
        .unwrap_or_default()
}

/// Return anchors attached to a specific SSLContext.  Unlike the selected
/// context slot, this remains available after an SSL factory crosses into a
/// different Java thread.
pub fn context_trust_root_ders(
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
pub fn is_dsa_private_key_pem(key_pem: &str) -> bool {
    openssl::pkey::PKey::private_key_from_pem(key_pem.as_bytes()).is_ok_and(|key| key.dsa().is_ok())
}

#[cfg(unix)]
pub fn is_dsa_certificate_der(der: &[u8]) -> bool {
    openssl::x509::X509::from_der(der)
        .ok()
        .and_then(|cert| cert.public_key().ok())
        .is_some_and(|key| key.dsa().is_ok())
}

pub fn take_selected_context_trust_roots() -> Option<TlsTrustRoots> {
    SELECTED_CONTEXT_TRUST_ROOTS.with(|c| c.borrow_mut().take())
}

pub fn ctx_identity_table() -> &'static Mutex<HashMap<u64, (String, String)>> {
    static T: OnceLock<Mutex<HashMap<u64, (String, String)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn ctx_trust_roots_table() -> &'static Mutex<HashMap<u64, TlsTrustRoots>> {
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
pub fn ctx_trust_managers_table() -> &'static Mutex<HashMap<u64, Vec<ObjectRef>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<ObjectRef>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn ctx_obj_key(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Result<u64, MethodCallFailed> {
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
/// `HUC_DEFAULTS` caches a whole config for the same reason on
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
pub fn ctx_client_session_store_table() -> &'static cratonvm_types::lock_order::OrderedPlMutex<
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
pub fn ctx_client_config_table(
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

pub fn ctx_client_session_store(key: u64) -> Arc<dyn rustls::client::ClientSessionStore> {
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
pub struct TracingClientSessionStore {
    pub inner: Arc<dyn rustls::client::ClientSessionStore>,
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
pub fn ctx_server_session_store_table() -> &'static cratonvm_types::lock_order::OrderedPlMutex<
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
pub fn ctx_server_session_store(
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
pub struct TracingServerSessionStore {
    pub inner: Arc<dyn rustls::server::StoresServerSessions + Send + Sync>,
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
pub fn attach_trust_managers_to_ctx(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
    tms_array: Option<ObjectRef>,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    // gc-common w13-a: `ctx_obj` is current here (both callers re-read it
    // from a handle), before anything below allocates.
    note_ssl_context_owner(&*ctx, key, ctx_obj);
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
pub fn ctx_jsse_identifies_table(
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

pub fn ctx_jsse_identifies(ctx_key: Option<u64>) -> bool {
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
pub fn ctx_accepted_issuers_table() -> &'static Mutex<HashMap<u64, Vec<Vec<u8>>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<Vec<u8>>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Accepted-issuer DNs recorded for `ctx_key`, as rustls
/// `DistinguishedName`s ready to go into a `CertificateRequest`.
pub fn accepted_issuer_hints(ctx_key: Option<u64>) -> Vec<rustls::DistinguishedName> {
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
pub fn capture_accepted_issuer_dns(
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
            // Bound by type (gc-common w20-e): each object below is used
            // straight after the call that produced it.
            let principal: ObjectRef = match ctx.invoke_virtual(
                cert,
                "getSubjectX500Principal",
                "()Ljavax/security/auth/x500/X500Principal;",
                &[],
            ) {
                Ok(Some(Value::Object(Some(p)))) => p,
                _ => continue,
            };
            let encoded: ObjectRef = match ctx.invoke_virtual(principal, "getEncoded", "()[B", &[])
            {
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
pub fn ctx_trust_managers_key_if_attached(
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
pub fn ctx_key_managers_table() -> &'static Mutex<HashMap<u64, Vec<ObjectRef>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<ObjectRef>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `SSLContext.init(km, tms, random)` calls this with the raw `km` array
/// argument (may be `None`/empty) to stash the actual `KeyManager` Java
/// objects against this context, keyed the same way as
/// `attach_trust_managers_to_ctx`.
pub fn attach_key_managers_to_ctx(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
    kms_array: Option<ObjectRef>,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    note_ssl_context_owner(&*ctx, key, ctx_obj);
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
///
/// `resolved_tm_trust_roots` is the trust-side counterpart, resolved DIRECTLY
/// from the actual `TrustManager[]` this call received (via
/// `x509_manager::resolved_trust_roots_for_trust_manager_array`) — same
/// reason, same fix shape, same priority over the thread-local
/// `PENDING_TM_TRUST_ROOTS`: see that function's doc and
/// jdk-only/P4A-TOMCAT-20260812-RETIRED-20260921.md defect 4 for the measured order-dependence this
/// closes (`TrustOrderProbe` — a `trusting` and a `defaulting` `SSLContext`
/// built from separate `TrustManagerFactory`s before either is initialised;
/// whichever `SSLContext.init` runs first drained the OTHER one's staged
/// roots). `take_pending_tm_trust_roots` is still called unconditionally so
/// it never leaks into a later, unrelated `SSLContext.init` either.
///
/// `km_supplied` is whether the call received a non-empty `KeyManager[]`
/// (nio20 wave 4, L4). Without one, SunJSSE's `chooseKeyManager(null)` builds
/// an EMPTY key manager (`KeyManagerFactory.init(null, null)`), so the context
/// has no identity whatever `KeyStore.load` staged on this thread before: the
/// staged identity used to become this context's, and a client context built
/// with `init(null, tms, null)` presented a certificate it was never given
/// (`NioConf19b clientauth-missing` completed a `setNeedClientAuth(true)`
/// handshake HotSpot fails with `SSLHandshakeException`). The staged identity
/// is still drained.
///
/// `tm_supplied` is the trust-side mirror (nio20 wave 5, L5;
/// `r20-compatible-jsse-carrier-context-init-and-alert-residuals`): without a
/// non-empty `TrustManager[]`, SunJSSE's `chooseTrustManager(null)` is the
/// DEFAULT trust manager (`TrustManagerFactory.init((KeyStore) null)`:
/// `javax.net.ssl.trustStore`, else the JDK's `cacerts`), never the anchors a
/// preceding `KeyStore.load` / `TrustManagerFactory.init` staged on this
/// thread. So the staged roots are drained and NOT used; the context gets the
/// default store's anchors ([`crate::tls::default_trust_store_keystore_id`]:
/// `javax.net.ssl.trustStore`, `jssecacerts`, `cacerts`, as
/// `TrustManagerFactory.init(null)` resolves them since nio21 wave 13), else,
/// with no readable store, no scoped roots, which every consumer reads as the
/// platform roots. A `TrustManager[]` that resolves no roots (a custom
/// `X509TrustManager`) keeps the staged-roots fallback, unchanged.
pub fn attach_pending_identity_to_ctx(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
    resolved_km_identity: Option<(String, String)>,
    resolved_tm_trust_roots: Option<(Vec<Vec<u8>>, Option<crate::x509_manager::RevocationConfig>)>,
    km_supplied: bool,
    tm_supplied: bool,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    note_ssl_context_owner(&*ctx, key, ctx_obj);
    let pending = take_pending_km_identity();
    let identity = if km_supplied {
        resolved_km_identity.or(pending)
    } else {
        None
    };
    if let Some(ident) = identity {
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
    let pending_roots = take_pending_tm_trust_roots();
    let roots = if tm_supplied {
        resolved_tm_trust_roots
            .map(|(root_ders, revocation)| TlsTrustRoots {
                root_ders: dedup_root_ders(root_ders),
                revocation,
            })
            .or(pending_roots)
    } else {
        default_trust_manager_roots(ctx)
    };
    if let Some(roots) = roots {
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] attach_pending_identity_to_ctx key={} storing {} roots",
                key,
                roots.root_ders.len()
            );
        }
        ctx_trust_roots_table().lock().insert(key, roots);
    } else {
        // A re-`init` replaces the context's managers wholesale (JSSE), so a
        // previous init's roots must not survive into this one.
        ctx_trust_roots_table().lock().remove(&key);
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] attach_pending_identity_to_ctx key={} NO pending roots to store",
                key
            );
        }
    }
    Ok(())
}

/// `SSLContext.getDefault()`'s lazily created context trusts what SunJSSE's
/// default trust manager trusts (`javax.net.ssl.trustStore`, else `cacerts`),
/// the same set `init(null, null, null)` scopes to. Without this the default
/// context read the platform roots and ignored the `trustStore` property
/// (nio21 wave 13, L4; `Nio21TrustDefault`). Unlike
/// [`attach_pending_identity_to_ctx`] it leaves the thread's staged
/// `KeyStore.load` identity and roots alone: no `init` is running.
///
/// Resolving the store reads a system property and may allocate, so the
/// context is pinned across the call and the (possibly moved) reference is
/// returned; callers use the returned one.
pub fn scope_default_context_trust(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    note_ssl_context_owner(&*ctx, key, ctx_obj);
    let pin = ctx.pin_native_root(ctx_obj);
    let roots = default_trust_manager_roots(ctx);
    let ctx_obj = ctx.read_native_pin(pin, ctx_obj);
    ctx.unpin_native_roots(pin);
    match roots {
        Some(roots) => {
            ctx_trust_roots_table().lock().insert(key, roots);
        }
        None => {
            ctx_trust_roots_table().lock().remove(&key);
        }
    }
    Ok(ctx_obj)
}

/// The anchors of SunJSSE's default trust manager as this VM scopes them: the
/// store `TrustManagerFactory.init((KeyStore) null)` resolves
/// (`javax.net.ssl.trustStore`, else `jssecacerts`, else the JDK's `cacerts`;
/// none for `NONE`). Only when no store resolves (no readable file) is it
/// `None`, which the client / server config builders read as the platform
/// roots. nio21 wave 13 (L4): this used the application-named store only, so
/// `init(.., null, ..)` trusted the OS roots where
/// `TrustManagerFactory.init(null)` and HotSpot trust `cacerts`
/// (`r20-compatible-ssl-context-null-trust-managers-use-platform-roots`). See
/// `attach_pending_identity_to_ctx`'s `tm_supplied`.
pub fn default_trust_manager_roots(ctx: &mut dyn NativeContext) -> Option<TlsTrustRoots> {
    let id = crate::tls::default_trust_store_keystore_id(ctx);
    if id == 0 {
        return None;
    }
    let state = crate::x509_manager::build_trust_manager_state(id);
    if state.anchor_ders.is_empty() {
        return None;
    }
    Some(TlsTrustRoots {
        root_ders: dedup_root_ders(state.anchor_ders),
        revocation: None,
    })
}

/// Look up the identity previously associated with an `SSLContext` object.
pub fn ctx_identity(
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
///
/// `trust_roots` is explicit (gc-common w10-e): its one caller,
/// `huc_capture_for_context`, has already taken the selected context's roots,
/// which is what the old ambient `active_client_trust_roots()` read resolved
/// to on that path. Private since then: `TlsTrustRoots` is this module's.
pub fn build_engine_client_config_with_identity(
    trust_roots: Option<TlsTrustRoots>,
    alpn: &[&str],
    client_identity: Option<(&str, &str)>,
    km_ctx_key: Option<u64>,
    trust_managers_ctx_key: Option<u64>,
) -> Result<Arc<ClientConfig>, String> {
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
pub fn client_config_for_ssl_context(
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
pub fn client_config_for_ssl_context_with_ciphers(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
    enabled_ciphers: &[String],
) -> Result<Arc<ClientConfig>, String> {
    let key = ctx_obj_key(ctx, ctx_obj)
        .map_err(|_| "--real-jdk refused a class this TLS context needs".to_string())?;
    let identity = ctx_identity(ctx, ctx_obj)
        .map_err(|_| "--real-jdk refused a class this TLS context needs".to_string())?;
    build_engine_client_config_with_identity_ciphers_in_vm(
        ctx.vm_identity(),
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
///
/// The trust roots are this thread's selected context's, else VM `vm`'s
/// `HttpsURLConnection` default roots (gc-common w10-e: those used to be one
/// set per process).
pub fn build_engine_client_config_with_identity_ciphers_in_vm(
    vm: usize,
    alpn: &[&str],
    client_identity: Option<(&str, &str)>,
    km_ctx_key: Option<u64>,
    trust_managers_ctx_key: Option<u64>,
    enabled_ciphers: &[String],
    enabled_protocols: &[String],
) -> Result<Arc<ClientConfig>, String> {
    build_engine_client_config_from_roots(
        active_client_trust_roots(vm),
        alpn,
        client_identity,
        km_ctx_key,
        trust_managers_ctx_key,
        enabled_ciphers,
        enabled_protocols,
        false,
    )
}

/// As [`build_engine_client_config_with_identity_ciphers_in_vm`], for a plain `SSLSocket` the VM connects
/// before the application can set its `SSLParameters`: the chain is still verified against the roots, but the
/// host NAME is not, because JSSE checks it only when the socket's parameters carry an endpoint
/// identification algorithm (`SSLSocket.startHandshake` applies one that was set afterwards, see
/// `ssl_security::new13_enforce_endpoint_identification`).
/// Build a deferred socket from the trust material captured by its own factory.
pub struct CapturedSocketConfig {
    pub config: Arc<ClientConfig>,
    pub presented: Arc<std::sync::atomic::AtomicBool>,
    pub local_chain: Vec<Vec<u8>>,
}
impl CapturedSocketConfig {
    pub fn presented_chain(&self) -> Vec<Vec<u8>> {
        if self.presented.load(std::sync::atomic::Ordering::Relaxed) {
            self.local_chain.clone()
        } else {
            Vec::new()
        }
    }
}

pub fn socket_identity_for_context_key(key: u64) -> Option<(String, String)> {
    ctx_identity_table().lock().get(&key).cloned()
}

pub fn build_captured_socket_config(
    root_ders: &[Vec<u8>],
    java_trust: bool,
    protocols: &[String],
    identity: Option<(&str, &str)>,
) -> Result<CapturedSocketConfig, String> {
    let trust = TlsTrustRoots {
        root_ders: root_ders.to_vec(),
        revocation: None,
    };
    let roots = root_store_for_trust_roots(Some(&trust));
    let (provider, versions) = provider_and_versions(&[], protocols);
    let config = build_client_config_ex_with_provider(
        roots,
        &[],
        ClientAuthMode::Fixed(identity),
        None,
        java_trust,
        provider,
        &versions,
        None,
        None,
        true,
    )?;
    let presented = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let local_chain = match identity {
        Some((cert, _)) => parse_cert_chain_pem(cert)?
            .iter()
            .map(|d| d.as_ref().to_vec())
            .collect(),
        None => Vec::new(),
    };
    let mut config = (*config).clone();
    config.client_auth_cert_resolver = Arc::new(RecordingClientCertResolver {
        inner: config.client_auth_cert_resolver.clone(),
        presented: presented.clone(),
    });
    Ok(CapturedSocketConfig {
        config: Arc::new(config),
        presented,
        local_chain,
    })
}

pub fn build_socket_client_config_with_identity_ciphers_in_vm(
    vm: usize,
    alpn: &[&str],
    client_identity: Option<(&str, &str)>,
    km_ctx_key: Option<u64>,
    trust_managers_ctx_key: Option<u64>,
    enabled_ciphers: &[String],
    enabled_protocols: &[String],
) -> Result<Arc<ClientConfig>, String> {
    build_engine_client_config_from_roots(
        active_client_trust_roots(vm),
        alpn,
        client_identity,
        km_ctx_key,
        trust_managers_ctx_key,
        enabled_ciphers,
        enabled_protocols,
        true,
    )
}

/// The body of the two builders above, over an already-resolved root set.
pub fn build_engine_client_config_from_roots(
    trust_roots: Option<TlsTrustRoots>,
    alpn: &[&str],
    client_identity: Option<(&str, &str)>,
    km_ctx_key: Option<u64>,
    trust_managers_ctx_key: Option<u64>,
    enabled_ciphers: &[String],
    enabled_protocols: &[String],
    lenient_names: bool,
) -> Result<Arc<ClientConfig>, String> {
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
                lenient_names,
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
        lenient_names,
    )
}

// -----------------------------------------------------------------------------
// HttpsURLConnection client TLS policy: one default set PER VM
// -----------------------------------------------------------------------------
//
// The native HttpsURLConnection client (`http_url_connection::perform`) does
// not route through `SSLSocketFactory.createSocket`, so everything it needs
// from the `SSLContext` behind an installed factory -- the client identity for
// mTLS (e.g. `TestClientCertTls13`'s `getUrl`/`postUrl`), the scoped trust
// roots, the `KeyManager` / `TrustManager` context keys, and the finished
// rustls config -- is captured into a `HucTlsCapture` when the factory is
// installed.
//
// gc-common w10-e (`common-w9v-https-url-connection-defaults-are-one-per-
// process`): these five slots were five process-wide statics, so VM A's
// `HttpsURLConnection.setDefaultSSLSocketFactory` replaced the client
// identity, trust roots and cipher/protocol policy of VM B's next `https:`
// request. They are now one `HucTlsCapture` per VM, published whole: a reader
// can no longer observe the identity of one capture next to the config of
// another, and an instance-scoped capture no longer has to borrow and restore
// the default slots (which let a concurrent default-scoped request on another
// thread see the instance factory's policy for the duration).

/// What `HttpsURLConnection` takes from one `SSLContext`. Every field is
/// derived data: no heap `ObjectRef`, so nothing here is a GC root (the
/// `KeyManager` / `TrustManager` objects stay in the rooted
/// `ctx_key_managers_table` / `ctx_trust_managers_table`; these are keys).
#[derive(Clone, Default)]
pub struct HucTlsCapture {
    /// The client identity `(cert_pem, key_pem)` the context carried, if any.
    pub identity: Option<(String, String)>,
    /// The trust anchors scoped to the context; `None` means platform roots.
    pub trust_roots: Option<TlsTrustRoots>,
    /// The finished rustls config. A `ClientConfig` owns rustls's client-side
    /// session store and `HttpsURLConnection` makes a new connection per URL
    /// request, so rebuilding it per request would discard the TLS 1.3 ticket
    /// the next request needs. Cloning the `Arc` shares the store on purpose.
    pub client_config: Option<Arc<ClientConfig>>,
    /// The `ctx_key_managers_table` key of the context, when it has captured
    /// `KeyManager`s, so the client config consults the real
    /// `KeyManager.chooseClientAlias` (via `JavaKeyManagerResolver`) instead
    /// of only ever presenting the one fixed `identity`.
    pub km_ctx_key: Option<u64>,
    /// The `ctx_trust_managers_table` key of the context, when it has captured
    /// `TrustManager`s: `perform`'s post-handshake gate runs them.
    pub tm_ctx_key: Option<u64>,
    /// Keeps the rows behind `km_ctx_key` / `tm_ctx_key` alive for as long as
    /// any clone of this capture exists -- in a slot, or in an in-flight
    /// `perform` whose slot was replaced meanwhile (gc-common w13-a). The
    /// config above runs a pass-through verifier when `tm_ctx_key` is set, so
    /// losing the `TrustManager`s mid-request would accept any chain.
    #[allow(dead_code)] // held for its `Drop`
    pub ctx_rows: CtxKeyLease,
}

impl HucTlsCapture {
    /// `identity` in the borrowed shape the rustls config builders take.
    pub fn identity_pems(&self) -> Option<(&str, &str)> {
        self.identity
            .as_ref()
            .map(|(cert, key)| (cert.as_str(), key.as_str()))
    }
}

/// The capture of the factory installed by `setDefaultSSLSocketFactory`, one
/// per VM. Behind an `Arc` so a reader's clone is a refcount bump, not a copy
/// of the trust anchors.
pub static HUC_DEFAULTS: cratonvm_native_api::vm_scoped::VmScoped<Option<Arc<HucTlsCapture>>> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Instance-scoped captures (`HttpsURLConnection.setSSLSocketFactory`), per
/// VM, keyed by the connection's weak lock key ([`gc_stable_objref_key`]).
/// `seq` orders the rows so the bound evicts the OLDEST capture rather than
/// every capture at once.
///
/// gc-common w26-b: the key was the connection's bare identity hash and no
/// row was removed when its connection died, so a later connection -- or a
/// LIVE one -- with the same hash was handed another connection's instance
/// policy: its `TrustManager` key (a permissive one, say) and client
/// identity, the exact cross-connection leak this table exists to prevent. The
/// connection is now the weak owner of its row
/// ([`TlsOwnerKind::HucConnection`]), which goes when the connection dies.
#[derive(Default)]
pub struct HucConnectionCaptures {
    pub next_seq: u64,
    pub rows: HashMap<u64, (u64, Arc<HucTlsCapture>)>,
}

/// Per-VM bound on [`HUC_CONNECTION_CAPTURES`]. A row is written when a
/// connection's factory is set and nothing tells this table when the
/// connection dies, so without a bound a VM that sets a factory per request
/// grows it forever. Evicting a row only sends that connection back to the
/// default policy, and only the oldest of 256 is ever evicted (the table used
/// to CLEAR at 256, dropping the policy of every live connection).
pub const HUC_CONNECTION_CAPTURES_PER_VM: usize = 256;

pub static HUC_CONNECTION_CAPTURES: cratonvm_native_api::vm_scoped::VmScoped<HucConnectionCaptures> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// The `ctx_trust_managers_table` key of `ctx_obj`, when that context has
/// captured `TrustManager`s.
pub fn huc_trust_managers_ctx_key(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
) -> Result<Option<u64>, MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let has_managers = ctx_trust_managers_table()
        .lock()
        .get(&key)
        .map(|managers| !managers.is_empty())
        .unwrap_or(false);
    Ok(has_managers.then_some(key))
}

/// Pins `ctx_obj` across [`capture_huc_key_managers_ctx_key_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `ctx_obj` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub fn capture_huc_key_managers_ctx_key(
    ctx: &mut dyn NativeContext,
    ctx_obj: &mut ObjectRef,
) -> Result<Option<u64>, MethodCallFailed> {
    let w5_pin = ctx.pin_native_root(*ctx_obj);
    let w5_out = capture_huc_key_managers_ctx_key_body(ctx, *ctx_obj);
    *ctx_obj = ctx.read_native_pin(w5_pin, *ctx_obj);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// The `ctx_key_managers_table` key of `ctx_obj`, or `None` when this context
/// has no captured `KeyManager`s at all, so a plain non-mTLS client (or one
/// whose `SSLContext.init` passed a null/empty `KeyManager[]`) keeps falling
/// back to `identity`/no-client-auth instead of spuriously trying (and
/// failing) to consult an empty resolver.
pub fn capture_huc_key_managers_ctx_key_body(
    ctx: &mut dyn NativeContext,
    ctx_obj: ObjectRef,
) -> Result<Option<u64>, MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let has_kms = ctx_key_managers_table().lock().contains_key(&key);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] capture_huc_key_managers_ctx_key key={} has_kms={}",
            key, has_kms
        );
    }
    Ok(has_kms.then_some(key))
}

/// Capture all TLS state of the Java `SSLContext` behind an
/// `HttpsURLConnection` factory, WITHOUT publishing it. This also runs for
/// anonymous clients: `ctx_identity` transfers scoped trust roots even when it
/// returns no client certificate, and every context needs a stable
/// `ClientConfig` to retain TLS 1.3 tickets across URL requests.
///
/// Consumes this thread's selected-context trust roots, exactly as the old
/// slot-writing capture did.
pub fn huc_capture_for_context(
    ctx: &mut dyn NativeContext,
    ctx_obj: &mut ObjectRef,
) -> Result<HucTlsCapture, MethodCallFailed> {
    let identity = ctx_identity(ctx, *ctx_obj)?;
    let trust_roots = selected_context_trust_roots();
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] huc_capture_for_context capturing roots={:?}",
            trust_roots.as_ref().map(|r| r.root_ders.len())
        );
    }
    set_selected_context_trust_roots(None);
    let km_ctx_key = capture_huc_key_managers_ctx_key(ctx, ctx_obj)?;
    let tm_ctx_key = huc_trust_managers_ctx_key(ctx, *ctx_obj)?;
    let client_config = build_engine_client_config_with_identity(
        trust_roots.clone(),
        &["http/1.1"],
        identity
            .as_ref()
            .map(|(cert, key)| (cert.as_str(), key.as_str())),
        km_ctx_key,
        tm_ctx_key,
    )
    .ok();
    let ctx_rows = CtxKeyLease::new(km_ctx_key.into_iter().chain(tm_ctx_key));
    Ok(HucTlsCapture {
        identity,
        trust_roots,
        client_config,
        km_ctx_key,
        tm_ctx_key,
        ctx_rows,
    })
}

/// `setDefaultSSLSocketFactory`: capture `ctx_obj` and publish it as the
/// CALLING VM's default `HttpsURLConnection` TLS policy.
pub fn capture_huc_ssl_context(
    ctx: &mut dyn NativeContext,
    ctx_obj: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let vm = ctx.vm_identity();
    let capture = Arc::new(huc_capture_for_context(ctx, ctx_obj)?);
    HUC_DEFAULTS.with(vm, |slot| *slot = Some(capture));
    Ok(())
}

/// `setSSLSocketFactory`: capture an instance factory for ONE connection,
/// without touching the default policy. HttpsURLConnection's instance setter
/// is scoped to one connection; keeping its config in the default slot made a
/// later plain connection accept the previous connection's permissive
/// TrustManager.
///
/// The key is minted, and the connection recorded as the row's weak owner,
/// BEFORE the capture, which allocates: `connection` is not pinned, and its
/// weak lock key is the one thing that survives a move (the stop-the-world
/// epilogue re-addresses its slot and the owner row alike).
pub fn capture_huc_ssl_context_for_connection(
    ctx: &mut dyn NativeContext,
    connection: ObjectRef,
    ctx_obj: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let vm = ctx.vm_identity();
    let key = gc_stable_objref_key(ctx, connection);
    note_tls_weak_owner(ctx, TlsOwnerKind::HucConnection, key, connection);
    let capture = huc_capture_for_context(ctx, ctx_obj)?;
    if capture.client_config.is_none() {
        return Ok(());
    }
    let capture = Arc::new(capture);
    HUC_CONNECTION_CAPTURES.with(vm, |table| {
        if !table.rows.contains_key(&key) && table.rows.len() >= HUC_CONNECTION_CAPTURES_PER_VM {
            let oldest = table
                .rows
                .iter()
                .min_by_key(|(_, (seq, _))| *seq)
                .map(|(k, _)| *k);
            if let Some(oldest) = oldest {
                table.rows.remove(&oldest);
            }
        }
        let seq = table.next_seq;
        table.next_seq = seq.wrapping_add(1);
        table.rows.insert(key, (seq, capture));
    });
    Ok(())
}

/// The capture `setSSLSocketFactory` recorded for `connection`, if any.
pub fn huc_tls_for_connection(
    ctx: &dyn NativeContext,
    connection: ObjectRef,
) -> Option<Arc<HucTlsCapture>> {
    // A lookup: a connection nobody gave a factory is never keyed.
    let key = existing_objref_key(ctx, connection)?;
    HUC_CONNECTION_CAPTURES
        .peek(ctx.vm_identity(), |table| {
            table.rows.get(&key).map(|(_, capture)| Arc::clone(capture))
        })
        .flatten()
}

/// VM `vm`'s default capture (`setDefaultSSLSocketFactory`), if one was made.
pub fn huc_default_tls_in_vm(vm: usize) -> Option<Arc<HucTlsCapture>> {
    HUC_DEFAULTS.peek(vm, |slot| slot.clone()).flatten()
}

/// VM `vm`'s default client identity.
pub fn huc_default_client_identity_in_vm(vm: usize) -> Option<(String, String)> {
    huc_default_tls_in_vm(vm).and_then(|capture| capture.identity.clone())
}

/// VM `vm`'s default trust roots: the fallback of every client config built
/// on a thread with no selected context.
pub fn huc_default_trust_roots_in_vm(vm: usize) -> Option<TlsTrustRoots> {
    huc_default_tls_in_vm(vm).and_then(|capture| capture.trust_roots.clone())
}

/// Rebuild `capture`'s client config under a cipher / protocol restriction
/// (`http_url_connection::perform`, once `huc_client_tls_restrictions` found
/// one). Built from the SAME ingredients as `capture.client_config` -- client
/// identity, `KeyManager` key, trust-manager key and trust roots -- so
/// narrowing the handshake never costs mTLS or custom trust. A root set
/// selected on this thread still wins, as it did when this read the process
/// defaults.
pub fn build_huc_restricted_client_config(
    capture: Option<&HucTlsCapture>,
    enabled_ciphers: &[String],
    enabled_protocols: &[String],
) -> Result<Arc<ClientConfig>, String> {
    let trust_roots =
        take_selected_context_trust_roots().or_else(|| capture.and_then(|c| c.trust_roots.clone()));
    build_engine_client_config_from_roots(
        trust_roots,
        &["http/1.1"],
        capture.and_then(HucTlsCapture::identity_pems),
        capture.and_then(|c| c.km_ctx_key),
        capture.and_then(|c| c.tm_ctx_key),
        enabled_ciphers,
        enabled_protocols,
        false,
    )
}

/// Drop VM `vm`'s `HttpsURLConnection` captures (from `forget_vm_tls_state`).
pub fn forget_vm_huc_captures(vm: usize) {
    HUC_DEFAULTS.forget(vm);
    HUC_CONNECTION_CAPTURES.forget(vm);
}

// -----------------------------------------------------------------------------
// PEM helpers
// -----------------------------------------------------------------------------

/// Parse a PEM-encoded cert chain into a vec of DER cert bytes. Returns an
/// error if the input is empty or contains no CERTIFICATE blocks.
pub fn parse_cert_chain_pem(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs: Vec<_> = CertificateDer::pem_slice_iter(pem.as_bytes())
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
// `docs/feature-designs/native-dispatch-architecture.md`.
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
pub fn der_header_len(buf: &[u8], at: usize) -> Option<usize> {
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
pub fn der_value_len(buf: &[u8], at: usize) -> Option<usize> {
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
pub fn der_tlv(buf: &[u8], at: usize) -> Option<(usize, usize)> {
    let hdr = der_header_len(buf, at)?;
    let len = der_value_len(buf, at)?;
    let start = at.checked_add(hdr)?;
    let end = start.checked_add(len)?;
    (end <= buf.len()).then_some((start, end))
}

/// End offset (exclusive) of the whole TLV at `at`.
pub fn der_tlv_end(buf: &[u8], at: usize) -> Option<usize> {
    Some(der_tlv(buf, at)?.1)
}

/// `1.2.840.10045.2.1` (id-ecPublicKey), as it appears inside an
/// `AlgorithmIdentifier` — tag, length and contents.
pub const OID_ID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];

/// Encode one DER TLV.
pub fn der_tlv_encode(tag: u8, body: &[u8]) -> Vec<u8> {
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
pub fn cert_ec_public_key_bits(cert_der: &[u8]) -> Option<Vec<u8>> {
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
pub fn spki_ec_bits(buf: &[u8], at: usize) -> Option<Vec<u8>> {
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
pub fn ec_pkcs8_splice_public_key(key_der: &[u8], leaf_cert_der: &[u8]) -> Option<Vec<u8>> {
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
pub fn repair_ec_key_for_ring<'a>(
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
pub fn certified_key_from_der_repairing_ec(
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
pub fn parse_private_key_pem(pem: &str) -> Result<PrivateKeyDer<'static>, String> {
    PrivateKeyDer::pem_slice_iter(pem.as_bytes())
        .next()
        .transpose()
        .map_err(|e| format!("PEM key parse failed: {e}"))?
        .ok_or_else(|| "no private key block found in PEM".to_string())
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
pub fn b64_encode(data: &[u8]) -> String {
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
pub fn der_to_pem(label: &str, der: &[u8]) -> String {
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

pub fn root_store_for_trust_roots(trust_roots: Option<&TlsTrustRoots>) -> RootCertStore {
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

/// This thread's selected context roots (taken), else VM `vm`'s
/// `HttpsURLConnection` default roots.
pub fn active_client_trust_roots(vm: usize) -> Option<TlsTrustRoots> {
    let selected = take_selected_context_trust_roots();
    let result = selected.or_else(|| huc_default_trust_roots_in_vm(vm));
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
pub fn trust_roots_pem(trust_roots: Option<&TlsTrustRoots>) -> String {
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
pub fn sniff_private_key_pem_header(der: &[u8]) -> &'static str {
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
/// a key entry is present. Installs into VM `vm`'s slot only (gc-common w11-a).
pub fn install_identity_from_der(vm: usize, key_pkcs8_der: &[u8], chain_der: &[Vec<u8>]) {
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
    // a VM's `RUNTIME_TLS_IDENTITY` slot (process-global before gc-common
    // w11-a) is last-write-wins, so a
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
        if runtime_tls_identity(vm).is_some() {
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

    set_runtime_tls_identity(
        vm,
        Some(RuntimeTlsIdentity {
            cert_pem,
            key_pem,
            client_ca_pem: None,
        }),
    );
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
///
/// Read from the OS ONCE per process (gc-common w10-e). Every client config
/// built without scoped trust roots came through here, so every such
/// handshake re-enumerated the OS certificate store and re-parsed ~150
/// anchors. The JDK reads its trust store once too (at `TrustManagerFactory`
/// init, from `cacerts`), so this process-wide snapshot of an OS-wide input is
/// not per-VM state. An empty or failed read is not cached: the next caller
/// tries again.
pub fn load_native_root_store() -> Result<RootCertStore, String> {
    static PLATFORM_ROOTS: OnceLock<RootCertStore> = OnceLock::new();
    if let Some(store) = PLATFORM_ROOTS.get() {
        return Ok(store.clone());
    }
    let store = load_native_root_store_uncached()?;
    if !store.is_empty() {
        let _ = PLATFORM_ROOTS.set(store.clone());
    }
    Ok(store)
}

pub fn load_native_root_store_uncached() -> Result<RootCertStore, String> {
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
pub fn accepted_issuer_ders() -> &'static Vec<Vec<u8>> {
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
pub struct SniCertResolver {
    pub hosts: HashMap<String, Arc<CertifiedKey>>,
    pub fallback: Option<Arc<CertifiedKey>>,
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
pub fn identity_certified_key(
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
    pub fn certified_key_from_pem(cert_pem: &str, key_pem: &str) -> Result<Arc<CertifiedKey>, String> {
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
pub struct TlsServerListenerEntry {
    pub listener: TcpListener,
    pub config: TlsServerConfig,
    pub local_port: u16,
    /// Set by `rustls_listener_close` and polled by `rustls_server_accept`'s
    /// non-blocking accept loop (see that function's doc comment) — lets a
    /// concurrent `close()` unblock a thread parked in `accept()` without
    /// needing the `sreg()` mutex, which that thread cannot hold while
    /// blocked.
    pub closed: Arc<AtomicBool>,
}

#[derive(Clone)]
pub enum TlsServerConfig {
    Rustls(Arc<ServerConfig>),
    Native(native_tls::TlsAcceptor),
    #[cfg(unix)]
    LegacyDsa(SslAcceptor),
}

pub struct ServerRegistry {
    pub next_id: i32,
    pub listeners: HashMap<i32, TlsServerListenerEntry>,
    /// rustls-backed client stream table. Runs in parallel with the native-tls
    /// client table in servlet::SocketRegistry — rustls is only selected when
    /// the Java caller explicitly goes through the `SSLContext` path that
    /// requested ALPN / session resumption / etc. Plain
    /// `SSLSocketFactory.createSocket` continues to route through native-tls
    /// for backwards compatibility with NEW-13.
    pub client_streams: HashMap<i32, TlsClientStreamEntry>,
    /// Accepted (server-side) rustls streams. Each entry is an owned
    /// `StreamOwned<ServerConnection, TcpStream>` plus captured negotiated
    /// fields.
    pub server_streams: HashMap<i32, TlsServerStreamEntry>,
    /// gc-common w14-b: the VM (`NativeContext::vm_identity`) that opened each
    /// row of the three maps above, keyed by the same id. The registry is one
    /// per process, so without this a VM's teardown could not tell its own
    /// listeners and streams from another VM's, and a disposed VM kept its
    /// ports bound for the life of the host process
    /// (`common-w13d-rustls-and-plain-socket-registries-outlive-their-vm`).
    ///
    /// Written by `create_ssl_server_socket` (the listener), by
    /// [`rustls_server_accept_within`] (an accepted stream inherits its
    /// listener's VM), by [`drive_pending_layered_handshake`] (the VM that
    /// stashed the layered socket) and by [`rustls_claim_for_vm`] (a client
    /// stream, claimed by the caller of [`rustls_client_connect`], usually
    /// through `servlet::s2_claim_for_vm`). The two close paths remove the row
    /// with the socket; [`alloc_server_id`] drops a leftover before its id is
    /// minted again. Read by [`forget_vm_rustls_sockets`].
    pub owners: HashMap<i32, usize>,
}

pub struct TlsClientStreamEntry {
    /// Per-stream mutex, NOT guarded by `sreg()`. Blocking socket I/O on one
    /// TLS connection must never hold the process-wide registry lock — see
    /// `rustls_stream_read`'s doc comment.
    pub stream: Arc<Mutex<StreamOwned<ClientConnection, TcpStream>>>,
    /// A `try_clone`d handle on the same socket, reachable WITHOUT `stream`'s
    /// mutex. Added 2026-08-12 (W7-61), modelled exactly on
    /// `servlet::TlsEntry::raw`, which already existed for this purpose on the
    /// native-tls table. It is the only way `rustls_stream_close` can reach the
    /// socket while another thread is parked in a read on it — that thread
    /// holds both the mutex and an `Arc`, so neither `try_lock` nor dropping
    /// our own `Arc` touches the connection. `None` only if `try_clone` failed.
    pub raw: Option<TcpStream>,
    pub local_cert_chain_der: Vec<Vec<u8>>,
    pub peer_host: String,
    pub peer_port: u16,
    pub negotiated_protocol: String,
    pub negotiated_cipher: String,
    pub negotiated_alpn: Option<String>,
}

pub struct TlsServerStreamEntry {
    /// Per-stream mutex — see `TlsClientStreamEntry::stream`.
    pub stream: Arc<Mutex<TlsServerStream>>,
    /// See `TlsClientStreamEntry::raw`.
    pub raw: Option<TcpStream>,
    pub sni_hostname: Option<String>,
    pub negotiated_protocol: String,
    pub negotiated_cipher: String,
    pub negotiated_alpn: Option<String>,
}

pub enum TlsServerStream {
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
    pub fn tcp(&self) -> Option<&TcpStream> {
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
            owners: HashMap::new(),
        }
    }
}

pub static SERVER_REGISTRY: OnceLock<Mutex<ServerRegistry>> = OnceLock::new();

pub fn sreg() -> &'static Mutex<ServerRegistry> {
    SERVER_REGISTRY.get_or_init(|| Mutex::new(ServerRegistry::default()))
}

pub fn alloc_server_id(reg: &mut ServerRegistry) -> i32 {
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
    // gc-common w14-b: a new row starts unowned until its allocator says
    // otherwise, even if a leftover owner row for this id survived somehow.
    reg.owners.remove(&id);
    id
}

/// Does `id` name a live row in any of the three rustls tables?
pub fn rustls_id_is_live(reg: &ServerRegistry, id: i32) -> bool {
    reg.listeners.contains_key(&id)
        || reg.client_streams.contains_key(&id)
        || reg.server_streams.contains_key(&id)
}

/// gc-common w14-b: record that VM `vm` owns the already-registered rustls row
/// `id` (the RAW id, below `servlet::RUSTLS_SOCK_ID_BASE`). For the paths that
/// register a stream without a `ctx`, chiefly [`rustls_client_connect`], whose
/// callers claim the id it returns. `servlet::s2_claim_for_vm` forwards an id
/// at or above `RUSTLS_SOCK_ID_BASE` here, so a caller holding the offset id
/// needs only that one call. A no-op for an id that names no live row.
pub fn rustls_claim_for_vm(vm: usize, id: i32) {
    let mut reg = sreg().lock();
    if rustls_id_is_live(&reg, id) {
        reg.owners.insert(id, vm);
    }
}

/// gc-common w14-b: close and drop every rustls listener and stream VM `vm`
/// opened, the layered sockets it stashed but never handshook, and its
/// `SSLServerSocket` side rows. Called from `servlet::forget_vm_s2_sockets`
/// (the socket teardown `lib.rs::forget_vm_native_root_stores` runs), because
/// rustls ids share that id space above `servlet::RUSTLS_SOCK_ID_BASE`
/// (`common-w13d-rustls-and-plain-socket-registries-outlive-their-vm`).
///
/// Dropping a `TcpListener` closes it and releases its port; each stream's
/// `raw` duplicate is shut down first, as [`rustls_stream_close`] does, so a
/// straggler parked in a read wakes (on Unix) rather than holding the socket
/// open, and each listener's `closed` flag is raised so a straggler in
/// [`rustls_server_accept_within`]'s poll loop leaves it and drops its
/// duplicate of the listener. Teardown runs after the VM's threads have
/// stopped, so neither should exist. Other VMs' rows, and unowned rows, are
/// left alone. These tables hold OS handles and Rust data only, no heap
/// references, so no collector scans them.
///
/// Answers the RAW ids of the streams it dropped, so the caller can drop the
/// `servlet` read-ahead buffered under `RUSTLS_SOCK_ID_BASE + id`.
pub fn forget_vm_rustls_sockets(vm: usize) -> Vec<i32> {
    let mut listeners: Vec<(i32, TlsServerListenerEntry)> = Vec::new();
    let mut clients: Vec<TlsClientStreamEntry> = Vec::new();
    let mut servers: Vec<TlsServerStreamEntry> = Vec::new();
    let mut stream_ids: Vec<i32> = Vec::new();
    {
        let mut reg = sreg().lock();
        let mut ids: Vec<i32> = Vec::new();
        reg.owners.retain(|id, owner| {
            if *owner == vm {
                ids.push(*id);
                false
            } else {
                true
            }
        });
        for id in ids {
            if let Some(l) = reg.listeners.remove(&id) {
                listeners.push((id, l));
            }
            if let Some(c) = reg.client_streams.remove(&id) {
                clients.push(c);
                stream_ids.push(id);
            }
            if let Some(s) = reg.server_streams.remove(&id) {
                servers.push(s);
                stream_ids.push(id);
            }
        }
    }
    // Stashed layered sockets hold a connected `TcpStream` until their first
    // handshake; a disposed VM's never come.
    let stale_pending: Vec<PendingLayeredSocket> = {
        let mut pending = pending_layered_sockets().lock();
        let ids: Vec<i32> = pending
            .iter()
            .filter(|(_, p)| p.vm == vm)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| pending.remove(&id))
            .collect()
    };
    if !listeners.is_empty() {
        let mut identities = sss_listener_identities().lock();
        for (id, _) in &listeners {
            identities.remove(id);
        }
    }
    // The `SSLServerSocket` state rows are keyed by the socket's weak lock key
    // (`gc_stable_objref_key`; gc-common w26-b), whose owning VM the lock-key
    // registry still answers here (TLS teardown runs before the lock keys').
    ssl_server_socket_states()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    sss_client_auth_states()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    // Outside every lock: a shutdown is a syscall.
    for (_, l) in &listeners {
        l.closed.store(true, Ordering::SeqCst);
    }
    for raw in clients
        .iter()
        .filter_map(|c| c.raw.as_ref())
        .chain(servers.iter().filter_map(|s| s.raw.as_ref()))
    {
        let _ = raw.shutdown(std::net::Shutdown::Both);
    }
    drop(listeners);
    drop(clients);
    drop(servers);
    drop(stale_pending);
    stream_ids
}

// -----------------------------------------------------------------------------
// Server config builders
// -----------------------------------------------------------------------------

/// Build a rustls `ServerConfig` for the single-cert case.
/// Honors ALPN advertisement and (optionally) requires a client certificate.
pub fn build_server_config_single_cert(
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
pub fn build_server_config_single_cert_ex(
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
pub fn server_builder_with_versions(
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
pub fn build_server_config_single_cert_ex_ciphers(
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
pub fn build_server_config_sni(
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
pub fn build_client_config(
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
pub struct OcspAwareServerCertVerifier {
    pub inner: Arc<dyn rustls::client::danger::ServerCertVerifier>,
    pub revocation: crate::x509_manager::RevocationConfig,
}

/// Verifies the chain exactly as `inner` does, but lets a certificate whose NAMES do not match the host
/// through. JSSE checks the host name of a plain `SSLSocket` only when its `SSLParameters` carry an endpoint
/// identification algorithm (`X509TrustManagerImpl.checkIdentity` is skipped for `null`); the VM connects
/// that socket before the parameters can be set, so the name check is applied afterwards, at
/// `startHandshake`, when the algorithm asks for it. webpki checks the chain before the name, so a
/// `NotValidForName` answer means the chain itself is trusted.
#[derive(Debug)]
pub struct NameLenientServerCertVerifier {
    pub inner: Arc<dyn rustls::client::danger::ServerCertVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for NameLenientServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName
                | rustls::CertificateError::NotValidForNameContext { .. },
            )) => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            other => other,
        }
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

/// A cryptographic-only verifier used when the caller supplied Java
/// `TrustManager`s. Chain and hostname policy is then applied immediately
/// after the handshake by `run_client_trust_check_for_chain`; this preserves
/// custom managers such as Tomcat's test-only `TrustAllCerts` without making
/// their policy invisible to the native HttpURLConnection path.
#[derive(Debug)]
pub struct PassthroughServerCertVerifier {
    pub algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
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
    /// the engine's (non-reentrant) lock -- `engine_registry()`'s write lock
    /// before gc-common w11-a, see [`EngineCell`] -- across the record loop,
    /// and an `X509ExtendedTrustManager` handed the `SSLEngine` may call
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
    pub endpoint_identity: Option<(String, String)>,
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
    pub trust_ctx_key: Option<u64>,
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
            // nio20 wave 4 (L4): an unchecked exception out of the manager is
            // JSSE's `SSLException("(internal_error) Unhandled exception")`,
            // not a certificate rejection. nio20 wave 5 (L5): the payload's
            // `(internal_error) ` prefix makes the vendored rustls send JSSE's
            // `internal_error` alert (it sent `handshake_failure`).
            Some(Ok(TrustOutcome::Unhandled(detail))) => {
                mark_trust_check_done(engine_id);
                if crate::nbflags().dbg_tls_auth_ok {
                    eprintln!("[dbg-tls-auth] (in-handshake) TrustManager threw: {detail}");
                }
                Err(rustls::Error::General(TRUST_MANAGER_UNHANDLED.to_string()))
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
pub fn build_client_config_with_revocation(
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
pub fn build_client_config_with_revocation_and_resolver(
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
pub enum ClientAuthMode<'a> {
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
pub fn build_client_config_ex(
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
        false,
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
pub fn build_client_config_ex_with_provider(
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
    // `lenient_names`: verify the chain against `roots` but accept a certificate whose NAMES do not match
    // the host (see `NameLenientServerCertVerifier`). Only a plain `SSLSocket` asks for it.
    lenient_names: bool,
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
                let mut inner: Arc<dyn rustls::client::danger::ServerCertVerifier> =
                    rustls::client::WebPkiServerVerifier::builder(roots)
                        .build()
                        .map_err(|e| format!("WebPkiServerVerifier::builder failed: {e}"))?;
                if lenient_names {
                    inner = Arc::new(NameLenientServerCertVerifier { inner });
                }
                let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> =
                    Arc::new(OcspAwareServerCertVerifier { inner, revocation });
                with_versions!(ClientConfig::builder_with_provider(provider.clone()))
                    .dangerous()
                    .with_custom_certificate_verifier(verifier)
            }
            None if lenient_names => {
                let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
                    .build()
                    .map_err(|e| format!("WebPkiServerVerifier::builder failed: {e}"))?;
                let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> =
                    Arc::new(NameLenientServerCertVerifier { inner });
                with_versions!(ClientConfig::builder_with_provider(provider))
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
    pub static ACTIVE_TLS_NATIVE_CTX: std::cell::Cell<Option<*mut (dyn NativeContext + 'static)>> =
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
    pub static ACTIVE_TLS_ENGINE: std::cell::Cell<Option<(i32, usize, ObjectRef)>> =
        const { std::cell::Cell::new(None) };
}

/// RAII: publish `engine` as the engine currently unwrapping on this thread.
pub struct ActiveEngineObjGuard {
    pub _private: (),
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
pub fn set_active_engine_binding(
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
pub fn active_engine_binding(ctx: &mut dyn NativeContext) -> Option<(i32, ObjectRef)> {
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
pub fn mark_trust_check_done(id: i32) {
    with_engine(id, |s| s.trust_check_done = true);
}

/// RAII guard returned by `set_active_native_context`; clears the
/// thread-local on drop (including on an early return/`?` inside the
/// handshake loop), so the raw pointer never outlives the native call frame
/// that created it.
pub struct ActiveNativeContextGuard {
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
    pub prev: Option<*mut (dyn NativeContext + 'static)>,
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
pub fn set_active_native_context(ctx: &mut dyn NativeContext) -> ActiveNativeContextGuard {
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
pub fn with_active_native_context<R>(f: impl FnOnce(&mut dyn NativeContext) -> R) -> Option<R> {
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
pub struct GcBlockedSyscall {
    pub entered: bool,
}

/// Open a [`GcBlockedSyscall`] region for the duration of the returned guard.
pub fn gc_blocked_syscall() -> GcBlockedSyscall {
    // The reborrow ends before the caller's syscall runs, so this never holds
    // a `&mut dyn NativeContext` across the blocking call — nor across the
    // `resolve` upcall, which reborrows it again from `process_new_packets`.
    // Interpreter round i3 wave 81, lane L6: the region is the socket
    // syscall itself, which HotSpot runs in a native (`SocketDispatcher.read0`
    // / `write0`): RUNNABLE in native.
    let entered = with_active_native_context(|ctx| {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative)
    })
    .is_some();
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
pub fn key_types_from_sigschemes(schemes: &[SignatureScheme]) -> Vec<String> {
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
pub fn build_issuer_principals(
    ctx: &mut dyn NativeContext,
    root_hint_subjects: &[&[u8]],
) -> Result<ObjectRef, MethodCallFailed> {
    if crate::phases_late::ssl_security::real_x500_principal(&*ctx) {
        // SBR wave 4 (lane SSL): `X500Principal`'s natives are retired, so a
        // 1-slot synthetic would NPE in the real `equals`/`getName` a real
        // `X509KeyManager.chooseClientAlias` runs. Real principals, built from
        // the hint's DER by `X500Principal(byte[])`.
        let cls_id = ctx
            .ensure_class_initialized("java/security/Principal")
            .unwrap_or(cratonvm_types::ClassId::new(0));
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let mut principals = Vec::new();
        for der in root_hint_subjects {
            if let Some(p) =
                crate::phases_late::ssl_security::new_real_x500_principal_from_der(&mut *scope, der)
            {
                principals.push(scope.root(p));
            }
        }
        let arr = scope.new_ref_array(cls_id, principals.len());
        for (i, h) in principals.iter().enumerate() {
            let p = scope.get(h);
            scope.set_array_element(arr, i, Value::Object(Some(p)));
        }
        return Ok(arr);
    }
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
///
/// The one `String[]` builder for this file's `()[Ljava/lang/String;` natives
/// (i6-L2): typed `String[]`, never `Object[]`, because JDK code clones these
/// arrays and `checkcast`s the clone (`SSLParameters.setProtocols`).
pub fn materialize_java_string_array<S: AsRef<str>>(
    ctx: &mut dyn NativeContext,
    items: &[S],
) -> ObjectRef {
    let cls_id = ctx
        .ensure_class_initialized("java/lang/String")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(cls_id, items.len());
    let arr_h = scope.root(arr);
    for (i, s) in items.iter().enumerate() {
        let js = scope.create_string(s.as_ref());
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(js)));
    }
    scope.get(&arr_h)
}

/// True if `result` is an `Err(ExceptionThrown)` wrapping a real
/// `java.lang.AbstractMethodError`. See `resolve_via_java`'s retry-on-first-
/// hit call site for why this specific exception gets a bounded retry
/// instead of being treated as a genuine dispatch failure.
pub fn is_abstract_method_error(
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
pub struct JavaKeyManagerResolver {
    /// Key into `ctx_key_managers_table` — never the `ObjectRef`s themselves.
    pub km_ctx_key: u64,
    pub provider: Arc<rustls::crypto::CryptoProvider>,
}

pub fn resolve_key_material_for_key(
    ctx: &mut dyn NativeContext,
    km_ctx_key: u64,
    root_hint_subjects: &[&[u8]],
    key_types: &[String],
) -> Result<Option<(Vec<Vec<u8>>, Vec<u8>)>, MethodCallFailed> {
    let dbg = crate::nbflags().dbg_tls_auth_ok;
    // Another VM's `KeyManager`s live in another heap (gc-common w9-b).
    if !ctx_key_visible_to(&*ctx, km_ctx_key) {
        return Ok(None);
    }
    let mut km_list = match ctx_key_managers_table().lock().get(&km_ctx_key) {
        Some(list) => list.clone(),
        None => return Ok(None),
    };
    if dbg {
        eprintln!(
            "[dbg-tls-auth] resolve_key_material_for_key km_ctx_key={} km_count={} root_hint_subjects={} key_types={:?}",
            km_ctx_key,
            km_list.len(),
            root_hint_subjects.len(),
            key_types
        );
    }
    if km_list.is_empty() {
        return Ok(None);
    }
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let key_type_arr = materialize_java_string_array(&mut *scope, key_types);
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
                if chain_der.is_empty() {
                    continue;
                }
                return Some((chain_der, key_der));
            }
            None
        })();

        ctx.unpin_native_roots(first_pin);
        Ok(result)
}

impl JavaKeyManagerResolver {
    /// Try each configured `KeyManager` in turn (matching real JSSE's
    /// `SSLContextImpl`, which does the same) until one's
    /// `chooseClientAlias` returns a non-null alias with resolvable
    /// cert/key material.
    pub fn resolve_via_java(
        &self,
        ctx: &mut dyn NativeContext,
        root_hint_subjects: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Result<Option<Arc<CertifiedKey>>, MethodCallFailed> {
        let key_types = key_types_from_sigschemes(sigschemes);
        let Some((chain_der, key_der)) =
            resolve_key_material_for_key(ctx, self.km_ctx_key, root_hint_subjects, &key_types)?
        else {
            return Ok(None);
        };
        let cert_chain: Vec<CertificateDer<'static>> =
            chain_der.into_iter().map(CertificateDer::from).collect();
        match certified_key_from_der_repairing_ec(cert_chain, key_der, &self.provider) {
            Ok(ck) => Ok(Some(Arc::new(ck))),
            Err(e) => {
                if crate::nbflags().dbg_tls_auth_ok {
                    eprintln!("[dbg-tls-auth] JavaKeyManagerResolver CertifiedKey::from_der failed: {e}");
                }
                Ok(None)
            }
        }
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
        // `--real-jdk` violation was recorded when the class was refused.
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
pub struct RecordingClientCertResolver {
    pub inner: Arc<dyn ResolvesClientCert>,
    pub presented: Arc<std::sync::atomic::AtomicBool>,
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

thread_local! {
    /// The "did this engine present a client certificate" flag of the engine
    /// whose inbound records THIS thread is feeding to rustls right now. Set by
    /// [`PresentedSink::enter`] around the two engine `process_new_packets`
    /// sites (`engine_unwrap_pump_raw`, `do_unwrap`'s record loop), read by
    /// [`SinkRecordingClientCertResolver`]. Per-thread scratch for one native
    /// call, not compatibility state: it is always restored on scope exit.
    pub static PRESENTED_SINK: std::cell::RefCell<Option<Arc<std::sync::atomic::AtomicBool>>> =
        const { std::cell::RefCell::new(None) };
}

/// Scope guard for [`PRESENTED_SINK`]: installs `flag`, restores the previous
/// value on drop (a nested engine call on the same thread keeps its own).
pub struct PresentedSink(pub Option<Arc<std::sync::atomic::AtomicBool>>);

impl PresentedSink {
    pub fn enter(flag: Option<Arc<std::sync::atomic::AtomicBool>>) -> PresentedSink {
        PresentedSink(PRESENTED_SINK.with(|s| std::mem::replace(&mut *s.borrow_mut(), flag)))
    }
}

impl Drop for PresentedSink {
    fn drop(&mut self) {
        let prev = self.0.take();
        PRESENTED_SINK.with(|s| *s.borrow_mut() = prev);
    }
}

/// [`RecordingClientCertResolver`] for a client config SHARED by every engine
/// of one `SSLContext` (nio20 wave 4, L4).
///
/// rustls resumes a session only when the resuming config's
/// `client_auth_cert_resolver` is the SAME `Arc` the session was stored under
/// (`ClientSessionCommon::compatible_config`, pointer identity). The
/// per-engine `RecordingClientCertResolver` is a fresh `Arc` per engine, so a
/// client context WITH a key manager could never resume: `NioConf19b resume`
/// (two TLS 1.2 engines of one context) answered `false` where HotSpot
/// answers `true`. This resolver is built once per cached config and writes
/// the per-engine flag through [`PRESENTED_SINK`], which the engine's own
/// processing call installs, so the "was a certificate sent" signal stays
/// per engine while the `Arc` is shared.
#[derive(Debug)]
pub struct SinkRecordingClientCertResolver {
    pub inner: Arc<dyn ResolvesClientCert>,
}

impl ResolvesClientCert for SinkRecordingClientCertResolver {
    fn resolve(
        &self,
        root_hint_subjects: &[&[u8]],
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let out = self.inner.resolve(root_hint_subjects, sigschemes);
        if out.is_some() {
            PRESENTED_SINK.with(|s| {
                if let Some(flag) = s.borrow().as_ref() {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            });
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
pub fn suite_to_java_cipher_name_pub(suite: rustls::CipherSuite) -> String {
    suite_to_java_cipher_name(suite)
}

pub fn suite_to_java_cipher_name(suite: rustls::CipherSuite) -> String {
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
        // `docs/feature-designs/native-dispatch-architecture.md`)
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
pub fn java_cipher_name_to_suite(name: &str) -> Option<rustls::CipherSuite> {
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
        "TLS_DHE_RSA_WITH_AES_128_GCM_SHA256" => TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
        "TLS_DHE_RSA_WITH_AES_256_GCM_SHA384" => TLS_DHE_RSA_WITH_AES_256_GCM_SHA384,
        _ => return None,
    })
}

/// The protocol names CratonVM's TLS stack advertises as supported, newest
/// first — the same "one list, one spelling" rule as
/// [`SUPPORTED_CIPHER_SUITE_NAMES`] below, and for the same reason: there were
/// three hand-maintained copies and they disagreed.
///
/// `SSLSocket.getSupportedProtocols()` said `[TLSv1.3, TLSv1.2]`,
/// `SSLEngine.getSupportedProtocols()` said `[TLSv1.3, TLSv1.2, TLSv1.1]`, and
/// `SSLContext.getSupportedSSLParameters()` agreed with the engine — so the VM
/// contradicted ITSELF about its own capability, which
/// `L6TlsParamSweep` row 118 asks directly and read `false`.
///
/// `TLSv1.1` is in the list on purpose and the reason is not that the
/// handshake can negotiate it: rustls cannot, and the mapper below is
/// deliberately limited to TLS 1.2/1.3. Tomcat's `SSLUtilBase.getEnabled`
/// INTERSECTS a connector's configured protocol list with this advertised set
/// and silently drops anything missing, so omitting `TLSv1.1` destroys an
/// explicit `TLSv1.1+TLSv1.2` configuration rather than declining it. The
/// advertised set is the configuration API's vocabulary; what a handshake
/// will actually agree to is a separate question, answered by
/// `getEnabledProtocols`.
pub const SUPPORTED_PROTOCOL_NAMES: &[&str] = &["TLSv1.3", "TLSv1.2", "TLSv1.1"];

/// The Java cipher-suite names CratonVM's TLS stack advertises as supported,
/// in JSSE preference order. Single source of truth for
/// `SSLEngine.getSupportedCipherSuites`, `SSLSocket
/// .getSupportedCipherSuites` and `SSLContext.getSupportedSSLParameters()`,
/// which previously each kept their own hand-maintained copy and had already
/// drifted apart.
pub const SUPPORTED_CIPHER_SUITE_NAMES: &[&str] = &[
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
pub const JSSE_PACKET_BUFFER_SIZE: i32 = 16709;

/// `SSLSession.getApplicationBufferSize()` for a session that has NOT
/// negotiated. MEASURED: 16704, which is [`JSSE_PACKET_BUFFER_SIZE`] minus the
/// 5-byte TLS record header.
pub const JSSE_APPLICATION_BUFFER_SIZE_FRESH: i32 = 16704;

/// `SSLSession.getApplicationBufferSize()` after a completed handshake.
/// MEASURED: 16676 for TLS 1.3 under both `TLS_AES_128_GCM_SHA256` and
/// `TLS_AES_256_GCM_SHA384` — the value moves when a suite is negotiated but
/// not between suites, which is why one constant per STATE is enough and a
/// per-suite table is not needed. See the accessor's comment for the audit
/// that made raising this from the old 16384 safe.
pub const JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED: i32 = 16676;

/// `ring`'s default `CryptoProvider`, augmented with the T-CBC.1 CBC-mode
/// TLS1.2 suites (`crate::t27_tls_cbc`) that `ring` itself never implements —
/// see `rustls-cbc-cipher-suites-not-supported.md`.
/// Every call site that used to construct `rustls::crypto::ring::default_provider()`
/// directly now goes through this instead, so the CBC suites are negotiable
/// (not just reported) everywhere TLS connections get set up.
pub fn cbc_augmented_default_provider() -> rustls::crypto::CryptoProvider {
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
        rustls::SupportedCipherSuite::from(
            &crate::t27_tls_cbc::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
        ),
        rustls::SupportedCipherSuite::from(
            &crate::t27_tls_cbc::TLS_DHE_RSA_WITH_AES_256_GCM_SHA384,
        ),
    ]);
    provider.kx_groups.extend([
        &crate::t27_tls_cbc::FFDHE2048_KX_GROUP as &'static dyn rustls::crypto::SupportedKxGroup,
        &crate::t27_tls_cbc::FFDHE3072_KX_GROUP as &'static dyn rustls::crypto::SupportedKxGroup,
    ]);
    provider
}

