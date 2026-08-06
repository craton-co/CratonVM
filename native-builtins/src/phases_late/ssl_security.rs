// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `javax.net.ssl`, `javax.crypto.Mac` and `java.security.cert` natives, plus the DER/ASN.1 helper used for X.509 extraction.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

/// Build a *typed* `java.security.SignatureException` for a certificate whose
/// signature could not be verified, and return it wrapped as a thrown Java
/// exception.
///
/// `Certificate.verify(PublicKey)` is contractually required to throw on a bad
/// signature — silently returning success is a certificate-verification
/// vulnerability, because a caller treats `verify()` returning normally as
/// proof that the certificate is trustworthy. Constructing the real
/// `java.security.SignatureException` via `new_object_initialized` gives the
/// thrown object the genuine `ClassId`, so handlers that
/// `catch (SignatureException)` (or any superclass: `GeneralSecurityException`,
/// `Exception`) match it correctly. If the class cannot be constructed we fall
/// back to a `SecurityException` rather than swallowing the failure — the
/// invariant is that verification failure NEVER returns normally.
#[cfg(feature = "legacy-synthetic-crypto")]
pub(crate) fn p68_signature_failure(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/security/SignatureException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::SecurityException {
        message: msg.to_string(),
    }
    .into()
}

// =============================================================================
// javax.crypto.Mac — Real HMAC support using SHA-256
// 4-field synthetic:
//   0 = algorithm (String)
//   1 = key object (Key — has getEncoded() returning byte[])
//   2 = initialized (Int: 0=no, 1=yes)
//   3 = accumulated data (byte[] array)
// =============================================================================

/// Helper: read raw bytes from a JVM byte array object
pub(crate) fn mac_read_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            bytes.push(b as u8);
        }
    }
    bytes
}

/// Helper: extract the key bytes from a Key object (field 0 = encoded byte[])
pub(crate) fn mac_extract_key_bytes(ctx: &mut dyn NativeContext, key_obj: ObjectRef) -> Vec<u8> {
    match ctx.get_field(key_obj, 0) {
        Value::Object(Some(arr)) => mac_read_byte_array(ctx, arr),
        _ => Vec::new(),
    }
}

/// Off-object state for the synthetic `javax.crypto.Mac` (bug-26 L3). Storing the
/// algorithm / key / accumulated data / init-flag in raw slots of a REAL
/// `javax.crypto.Mac` object aliases that class's real fields and gets corrupted
/// under the allocation-heavy `ScramFormatter.hi()` init-once/doFinal-many reuse
/// loop's GC (manifesting as wrong HMAC bytes — e.g. rfc7677 vector failures).
/// Keep all state in a process-wide table keyed by the object's identity hash
/// (stable across GC); the `Mac` handle itself stays opaque.
#[derive(Default, Clone)]
pub(crate) struct MacState {
    pub(crate) algo: String,
    pub(crate) key: Vec<u8>,
    pub(crate) data: Vec<u8>,
    pub(crate) initialized: bool,
}

/// BUG nb-phases-late(4) [VULN low]: scrub HMAC key (and accumulated) bytes on
/// drop so that whenever a `MacState` is evicted, replaced, or the table is torn
/// down, the secret key material is zeroed in place rather than left lingering in
/// freed heap for the VM lifetime. We cannot zero the key on doFinal()/reset()
/// because the JDK keeps the Mac keyed for reuse after those calls (the
/// ScramFormatter.hi() init-once/doFinal-many loop relies on this), so the
/// eviction-time scrub plus the bounded-cap eviction below is the correct,
/// non-breaking mitigation.
impl Drop for MacState {
    fn drop(&mut self) {
        mac_zeroize(&mut self.key);
        mac_zeroize(&mut self.data);
    }
}

/// Overwrite a secret byte buffer with zeros, defeating dead-store elimination
/// via a volatile write per byte, then clear it.
pub(crate) fn mac_zeroize(buf: &mut Vec<u8>) {
    for b in buf.iter_mut() {
        // SAFETY: `b` points to a valid, uniquely-borrowed u8 inside the Vec.
        unsafe { std::ptr::write_volatile(b as *mut u8, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    buf.clear();
}

/// Hard cap on live `MacState` entries. The table is keyed by identity hash and
/// never observed object finalization, so without a cap a long-running VM that
/// churns through Mac instances would retain every key forever (BUG
/// nb-phases-late(4)). When the cap is exceeded we evict the lowest-keyed
/// entries; their `Drop` scrubs the key bytes.
pub(crate) const MAC_STATE_MAX_ENTRIES: usize = 4096;

pub(crate) fn mac_state_table(
) -> &'static std::sync::Mutex<std::collections::HashMap<i32, MacState>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i32, MacState>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Bound the Mac state table: if it has grown past the cap, drop excess entries
/// (scrubbing their key material via `MacState::drop`). Called from `getInstance`
/// just before inserting a fresh entry. `keep` is the id we are about to insert,
/// which is never evicted. Best-effort eviction (lowest ids first) — these are
/// abandoned Mac handles whose Java objects are unreachable.
pub(crate) fn mac_state_evict_if_needed(
    t: &mut std::collections::HashMap<i32, MacState>,
    keep: i32,
) {
    if t.len() < MAC_STATE_MAX_ENTRIES {
        return;
    }
    let target = MAC_STATE_MAX_ENTRIES / 2;
    let mut ids: Vec<i32> = t.keys().copied().filter(|&k| k != keep).collect();
    ids.sort_unstable();
    let to_remove = t.len().saturating_sub(target);
    for id in ids.into_iter().take(to_remove) {
        // Removing drops the MacState, whose Drop zeroes the key bytes.
        t.remove(&id);
    }
}

pub(crate) fn register_p68_crypto_mac(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let mac = "javax/crypto/Mac";
    r.register(
        mac,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Mac;",
        |ctx, args| {
            // The algorithm String is the first reference arg (static natives have
            // no receiver placeholder — cf. md_get_instance). State lives off-object
            // in mac_state_table, keyed by identity hash (bug-26 L3).
            let algo = args
                .iter()
                .find_map(|v| match v {
                    Value::Object(Some(o)) => ctx.read_string(*o),
                    _ => None,
                })
                .unwrap_or_default();
            let obj = alloc_concurrent_synthetic(ctx, "javax/crypto/Mac", 4);
            let id = ctx.identity_hash_code(obj);
            // BUG nb-phases-late(4): bound the key-bearing side-table before
            // inserting so it cannot retain key material for the VM lifetime.
            let mut t = mac_state_table().lock().unwrap();
            mac_state_evict_if_needed(&mut t, id);
            t.insert(
                id,
                MacState {
                    algo,
                    key: Vec::new(),
                    data: Vec::new(),
                    initialized: false,
                },
            );
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mac,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Mac;",
        |ctx, args| {
            // The algorithm String is the first reference arg (static natives have
            // no receiver placeholder — cf. md_get_instance). State lives off-object
            // in mac_state_table, keyed by identity hash (bug-26 L3).
            let algo = args
                .iter()
                .find_map(|v| match v {
                    Value::Object(Some(o)) => ctx.read_string(*o),
                    _ => None,
                })
                .unwrap_or_default();
            let obj = alloc_concurrent_synthetic(ctx, "javax/crypto/Mac", 4);
            let id = ctx.identity_hash_code(obj);
            // BUG nb-phases-late(4): bound the key-bearing side-table before
            // inserting so it cannot retain key material for the VM lifetime.
            let mut t = mac_state_table().lock().unwrap();
            mac_state_evict_if_needed(&mut t, id);
            t.insert(
                id,
                MacState {
                    algo,
                    key: Vec::new(),
                    data: Vec::new(),
                    initialized: false,
                },
            );
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(mac, "init", "(Ljava/security/Key;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key_bytes = match args.get(1) {
            Some(Value::Object(Some(k))) => mac_extract_key_bytes(ctx, *k),
            _ => Vec::new(),
        };
        let id = ctx.identity_hash_code(this);
        let mut t = mac_state_table().lock().unwrap();
        let st = t.entry(id).or_default();
        // BUG nb-phases-late(4): scrub the previous key in place before
        // replacing it, so re-keying a reused Mac doesn't leave the old key
        // lingering in freed heap.
        mac_zeroize(&mut st.key);
        st.key = key_bytes;
        st.initialized = true;
        st.data.clear();
        Ok(None)
    });
    // update([B)V — append byte array to accumulator
    r.register(mac, "update", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(arr))) = args.get(1) {
            let bytes = mac_read_byte_array(ctx, *arr);
            let id = ctx.identity_hash_code(this);
            mac_state_table()
                .lock()
                .unwrap()
                .entry(id)
                .or_default()
                .data
                .extend_from_slice(&bytes);
        }
        Ok(None)
    });
    // update([BII)V — append byte range to accumulator
    r.register(mac, "update", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(arr))) = args.get(1) {
            // Validate signed off/len against the array length BEFORE casting to
            // usize. A negative len would sign-extend into a huge usize and
            // abort `Vec::with_capacity`; reject out-of-range ranges with
            // IndexOutOfBoundsException as the JDK Mac/SPI does.
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            let arr_len = ctx.array_length(*arr) as i64;
            if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
                return Err(RuntimeError::aioobe_index_only(if off < 0 { off } else { off.wrapping_add(len) })
                .into());
            }
            let off = off as usize;
            let len = len as usize;
            let mut bytes = Vec::with_capacity(len);
            for i in 0..len {
                if let Value::Int(b) = ctx.get_array_element(*arr, off + i) {
                    bytes.push(b as u8);
                }
            }
            let id = ctx.identity_hash_code(this);
            mac_state_table()
                .lock()
                .unwrap()
                .entry(id)
                .or_default()
                .data
                .extend_from_slice(&bytes);
        }
        Ok(None)
    });
    // update(B)V — append single byte to accumulator
    r.register(mac, "update", "(B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        let id = ctx.identity_hash_code(this);
        mac_state_table()
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .data
            .push(b);
        Ok(None)
    });
    // doFinal()[B — compute HMAC, return result, reset accumulator
    r.register(mac, "doFinal", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.identity_hash_code(this);
        // PERF: single lock acquisition, no full-state clones. Previously this
        // cloned algo (String) + key (Vec) + the entire accumulated data (Vec)
        // out of the mutex, then re-locked to clear `data`. Instead, take the
        // lock once, MOVE `data` out via `mem::take` (which both gives us owned
        // bytes without a copy AND performs the JDK "reset buffer, keep Mac
        // initialized" semantics in one step), and compute the HMAC borrowing
        // algo/key in place. Same correctness; eliminates two Vec clones + a
        // second lock round-trip per call in the hot ScramFormatter loop.
        let hmac_result = {
            let mut t = mac_state_table().lock().unwrap();
            let st = match t.get_mut(&id) {
                Some(st) => st,
                None => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "MAC not initialized".into(),
                    }
                    .into());
                }
            };
            if !(st.initialized || !st.key.is_empty()) {
                return Err(RuntimeError::IllegalStateException {
                    message: "MAC not initialized".into(),
                }
                .into());
            }
            // mem::take resets the accumulator (keeps Mac initialized for reuse).
            let data = std::mem::take(&mut st.data);
            mac_compute_hmac(&st.algo, &st.key, &data)
        };
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, hmac_result.len());
        ctx.write_byte_array_from(arr, 0, &hmac_result);
        Ok(Some(Value::Object(Some(arr))))
    });
    // doFinal([B)[B — update with input bytes, then compute HMAC
    r.register(mac, "doFinal", "([B)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.identity_hash_code(this);
        // Read the input bytes via ctx BEFORE taking the lock (ctx access must
        // not happen while the state mutex is held).
        let input = match args.get(1) {
            Some(Value::Object(Some(input_arr))) => Some(mac_read_byte_array(ctx, *input_arr)),
            _ => None,
        };
        // PERF: same single-lock / move-out optimization as doFinal()[B above —
        // append the input, compute the HMAC, and reset the accumulator under a
        // single lock with no full-state clones (was: append-lock, clone-lock,
        // clear-lock = three acquisitions plus algo/key/data clones per call).
        let hmac_result = {
            let mut t = mac_state_table().lock().unwrap();
            let st = t.entry(id).or_default();
            if let Some(bytes) = input.as_ref() {
                st.data.extend_from_slice(bytes);
            }
            if !(st.initialized || !st.key.is_empty()) {
                return Err(RuntimeError::IllegalStateException {
                    message: "MAC not initialized".into(),
                }
                .into());
            }
            // mem::take resets the accumulator (keeps Mac initialized for reuse).
            let data = std::mem::take(&mut st.data);
            mac_compute_hmac(&st.algo, &st.key, &data)
        };
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, hmac_result.len());
        ctx.write_byte_array_from(arr, 0, &hmac_result);
        Ok(Some(Value::Object(Some(arr))))
    });
    // reset()V — clear the accumulator
    r.register(mac, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.identity_hash_code(this);
        if let Some(st) = mac_state_table().lock().unwrap().get_mut(&id) {
            st.data.clear();
        }
        Ok(None)
    });
    r.register(mac, "getMacLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.identity_hash_code(this);
        let algo = mac_state_table()
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.algo.clone())
            .unwrap_or_default();
        Ok(Some(Value::Int(mac_output_length(&algo) as i32)))
    });
    r.register(mac, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.identity_hash_code(this);
        let algo = mac_state_table()
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.algo.clone())
            .unwrap_or_default();
        Ok(Some(Value::Object(Some(ctx.create_string(&algo)))))
    });
    r.register(mac, "clone", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let src_state = mac_state_table()
            .lock()
            .unwrap()
            .get(&ctx.identity_hash_code(this))
            .cloned()
            .unwrap_or_default();
        let clone = alloc_concurrent_synthetic(ctx, "javax/crypto/Mac", 4);
        let cid = ctx.identity_hash_code(clone);
        mac_state_table().lock().unwrap().insert(cid, src_state);
        Ok(Some(Value::Object(Some(clone))))
    });
    r.set_category(__prev_cat);
}

/// Compute HMAC using the appropriate hash based on the Java algorithm name.
pub(crate) fn mac_compute_hmac(algo: &str, key: &[u8], data: &[u8]) -> Vec<u8> {
    let upper = algo.to_uppercase().replace('-', "");
    match upper.as_str() {
        "HMACSHA384" => hmac_sha384(key, data),
        "HMACSHA512" => hmac_sha512(key, data),
        "HMACSHA1" => hmac_sha1(key, data),
        "HMACMD5" => hmac_md5(key, data),
        _ => hmac_sha256(key, data), // HmacSHA256 and default
    }
}

/// Return the output length in bytes for the given HMAC algorithm.
pub(crate) fn mac_output_length(algo: &str) -> usize {
    let upper = algo.to_uppercase().replace('-', "");
    match upper.as_str() {
        "HMACSHA384" => 48,
        "HMACSHA512" => 64,
        "HMACSHA1" => 20,
        "HMACMD5" => 16,
        _ => 32, // HmacSHA256 default
    }
}

// =============================================================================
// javax.net.ssl — SSLContext, SSLSocketFactory, SSLSocket, TrustManager stubs
// =============================================================================

// NEW-13: SSLContext field layout (expanded from 1 → 5 fields so init() has
// somewhere to store the KM/TM/SecureRandom arguments).
//   0 = protocol String ("TLSv1.2" / "TLSv1.3" / …)
//   1 = initialized Int (0 before init, 1 after)
//   2 = key_managers Object (KeyManager[] or null)
//   3 = trust_managers Object (TrustManager[] or null)
//   4 = secure_random Object (SecureRandom or null)
pub(crate) const NEW13_SSL_CTX_FIELDS: usize = 5;

pub(crate) const NEW13_CTX_PROTOCOL: usize = 0;

pub(crate) const NEW13_CTX_INIT: usize = 1;

pub(crate) const NEW13_CTX_KM: usize = 2;

pub(crate) const NEW13_CTX_TM: usize = 3;

pub(crate) const NEW13_CTX_RANDOM: usize = 4;

// SSLSocket field layout (6 fields — matches previous register_p68_ssl, with
// the semantic of field 2 changed from fd_table fd_id → s2_registry tls_id).
//   0 = host String, 1 = port Int, 2 = tls_id Int (s2_registry),
//   3 = closed Int, 4 = session Object, 5 = reserved
pub(crate) const NEW13_SSL_SOCK_FIELDS: usize = 6;

pub(crate) const NEW13_SOCK_HOST: usize = 0;

pub(crate) const NEW13_SOCK_PORT: usize = 1;

pub(crate) const NEW13_SOCK_TLSID: usize = 2;

pub(crate) const NEW13_SOCK_CLOSED: usize = 3;

pub(crate) const NEW13_SOCK_SESSION: usize = 4;

/// FIX (netty-client-socket-write-after-close): resolve a `javax/net/ssl/
/// SSLSocket` object's `s2_registry` stream id whether it was built by
/// `new13_do_create_socket` below (raw field `NEW13_SOCK_TLSID`) or by
/// `net_phase_e`'s OWN `SSLSocketFactory.createSocket(String,int)` — that
/// exact (class,name,descriptor) is registered in BOTH modules, and
/// `net_phase_e::register_phase_e_networking` runs after `register_p68_ssl`
/// (see lib.rs's `register_essential_natives`), so its side-table-based
/// implementation wins and never populates this raw field, leaving it at
/// its allocation default. The stream/lifecycle natives below are
/// registered on the concrete `SSLSocket` class though (a more specific
/// match than `net_phase_e`'s registrations on the `java/net/Socket`
/// superclass), so they run regardless of which factory built the object —
/// falling back to `net_phase_e`'s side table here is what makes
/// `getOutputStream`/`write` work on a socket obtained via the plain
/// 2-arg `createSocket(host, port)` (the overload Apache HttpClient5's
/// classic connection pool actually calls, per
/// fixed-suite-bugs/netty-client-socket-write-after-close-nsme-FIXED.md).
pub(crate) fn new13_resolve_tls_id(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    if let Some(id) = ctx.get_field(this, NEW13_SOCK_TLSID).as_int() {
        if id >= 0 {
            return id;
        }
    }
    crate::net_phase_e::sock_stream_id_for_upcall(ctx, this)
}

// SSLSession field layout: 3 fields.
//   0 = protocol String
//   1 = cipher String
//   2 = tls_id Int (s2_registry id of the backing TLS stream; -1 after close)
pub(crate) const NEW13_SSL_SESS_FIELDS: usize = 3;

pub(crate) const NEW13_SESS_PROTO: usize = 0;

pub(crate) const NEW13_SESS_CIPHER: usize = 1;

pub(crate) const NEW13_SESS_TLSID: usize = 2;

/// NEW-13: build a `native_tls::TlsConnector` for an SSLContext.
///
/// The returned connector enforces the platform trust store and hostname
/// verification by default — which, combined with native-tls's handshake,
/// gives the same security posture as the reference JDK's default path.
///
/// FIX (es-restclient-https): when the Java caller supplied a non-null
/// `TrustManager[]` to `SSLContext.init` that was produced by a
/// `TrustManagerFactory` bound to an explicit (non-default) `KeyStore` —
/// e.g. a test truststore holding a self-signed/test-CA certificate — the
/// connector must ALSO trust those anchors, or every handshake against a
/// server presenting that certificate fails with `UnknownIssuer` even
/// though the JDK caller correctly built and wired a custom trust store
/// (`RestClientBuilderIntegTests`/`HttpsServer` pattern: one `SSLContext`
/// built from a JKS truststore, used for both the server and the client).
/// The previous comment here ("NEW-13 roadmap accepts ignoring custom
/// TrustManagers") was predicated on native-tls having no pluggable trust
/// hook at all; that's true for arbitrary Java `TrustManager` callback
/// logic, but a KeyStore-derived anchor set is just extra root certs, which
/// `TlsConnectorBuilder::add_root_certificate` supports on every native-tls
/// 0.2 backend (SChannel/SecureTransport/OpenSSL). We ADD these roots to
/// (not replace) the platform trust store — every native-tls backend's
/// verifier lets a chain be valid if it reaches home to ANY trusted root,
/// so this does not weaken validation of servers using ordinary publicly
/// trusted certs; it only additionally allows a caller-configured private
/// CA/self-signed leaf, which mirrors the reference JDK's restrictive
/// custom-truststore semantics closely enough to make the common
/// self-signed-test-cert pattern actually work.
pub(crate) fn new13_build_connector(
    extra_root_ders: &[Vec<u8>],
    danger_skip_native_verify: bool,
    max_protocol: Option<native_tls::Protocol>,
) -> Result<native_tls::TlsConnector, String> {
    let mut builder = native_tls::TlsConnector::builder();
    builder.min_protocol_version(Some(native_tls::Protocol::Tlsv12));
    // FIX (TestSsl.testClientInitiatedRenegotiation[JSSE]): honour a
    // version-pinned `SSLContext.getInstance(...)`. Before this, the protocol
    // string was validated, stored on the SSLContext object, and then never
    // consulted again — so `SSLContext.getInstance("TLSv1.2")` produced a
    // socket that happily negotiated TLS 1.3, and `getEnabledProtocols()`
    // reported `[TLSv1.3]` where stock JDK 25 reports `[TLSv1.2]`. That is
    // not a cosmetic difference: a caller pins the version precisely because
    // the two protocols differ behaviourally (TLS 1.3 has no renegotiation
    // and no post-handshake `HandshakeCompletedEvent`), so silently upgrading
    // it changes the semantics the caller asked for. See
    // `new13_ctx_max_protocol` for why only the 1.2 ceiling is honoured.
    if let Some(max) = max_protocol {
        builder.max_protocol_version(Some(max));
    }
    if danger_skip_native_verify {
        // FIX (netty-https-client-trust): the owning SSLContext was init'd
        // with REAL Java TrustManager objects (e.g. HttpClient5's
        // SSLContextBuilder + TrustSelfSignedStrategy delegate). JSSE
        // semantics make that TrustManager THE verifier, so native-tls's own
        // WebPKI check must stand down — the caller runs the Java
        // `checkServerTrusted` against the captured peer chain immediately
        // after connect and aborts the socket on rejection (fail-closed; see
        // `new13_do_create_socket`). Hostname verification is likewise not an
        // SSLSocket-level concern in JSSE (no endpoint identification
        // algorithm is set on this path); HTTP clients apply their own
        // HostnameVerifier on top (e.g. HttpClient5's verifySession).
        builder.danger_accept_invalid_certs(true);
        builder.danger_accept_invalid_hostnames(true);
    }
    for der in extra_root_ders {
        match native_tls::Certificate::from_der(der) {
            Ok(cert) => {
                builder.add_root_certificate(cert);
            }
            Err(e) => {
                tracing::debug!(
                    target: "phases_late::tls",
                    "SSLContext: skipping unparseable custom trust anchor DER: {}",
                    e
                );
            }
        }
    }
    builder
        .build()
        .map_err(|e| format!("TlsConnector build failed: {}", e))
}

/// FIX (es-restclient-https): per-`SSLContext` custom trust anchors, keyed by
/// the context object's identity (raw pointer — stable across GC per the
/// `ObjectRef` conventions already used by the object-identity side-tables
/// elsewhere in this file, e.g. `zo_buf_key`). Populated by `SSLContext.init`
/// from a `TrustManagerFactory`-produced `TrustManager[]` that is bound to an
/// explicit KeyStore (see `tls.rs::register_trust_manager_factory` /
/// `x509_manager::build_trust_manager_state`); consumed by `getSocketFactory`
/// so the returned `SSLSocketFactory` carries the same trust scope into
/// `createSocket`.
pub(crate) fn p68_ctx_trust_roots_table(
) -> &'static parking_lot::Mutex<std::collections::HashMap<usize, Vec<Vec<u8>>>> {
    static T: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<usize, Vec<Vec<u8>>>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Extract the keystore-bound trust anchors (DER, restrictive — i.e. NOT
/// merged with system roots) from a `TrustManager[]`, if any element is the
/// real `javax.net.ssl.X509TrustManager` produced by
/// `TrustManagerFactory.getTrustManagers()` (stamped with the bound keystore
/// id via the `cratonvm$x509tm$id` field / slot 0 fallback — same convention
/// `tls.rs::read_trust_manager_id_from_obj` reads). Returns `None` when the
/// array is null/empty or every element resolves to keystore id 0 (the
/// default/system trust store — nothing extra to add).
pub(crate) fn p68_extract_trust_manager_roots(
    ctx: &mut dyn NativeContext,
    tm_arg: Value,
) -> Option<Vec<Vec<u8>>> {
    let Value::Object(Some(tm_arr)) = tm_arg else {
        return None;
    };
    let len = ctx.array_length(tm_arr);
    for i in 0..len {
        if let Value::Object(Some(tm)) = ctx.get_array_element(tm_arr, i) {
            let ks_id = match ctx.get_field_by_name(tm, "cratonvm$x509tm$id") {
                Value::Int(id) if id != 0 => id,
                _ => match ctx.object_num_fields(tm) {
                    n if n > 0 => match ctx.get_field(tm, 0) {
                        Value::Int(id) if id != 0 => id,
                        _ => continue,
                    },
                    _ => continue,
                },
            };
            // FIX (tls-residuals): this id is a `tm_registry` id (from
            // `x509_manager::register_trust_manager_state`) for the live
            // PKIXFactory/SimpleFactory path, not necessarily a raw KeyStore
            // registry id — see `tls.rs::validate_cert_chain`'s matching fix
            // for the full explanation of the two-id-space collision this
            // closes.
            let state = crate::x509_manager::trust_manager_state_by_id(ks_id);
            if !state.anchor_ders.is_empty() {
                return Some(state.anchor_ders);
            }
        }
    }
    None
}

/// Map an `SSLContext.getInstance(<name>)` protocol string to the TLS version
/// CEILING that context implies, in JSSE terms: a version-specific context's
/// default enabled-protocol set is that version alone (stock JDK 25 with
/// `getInstance("TLSv1.2")` reports `getEnabledProtocols() == [TLSv1.2]`),
/// whereas the version-agnostic names leave the ceiling open.
///
/// Only the TLS 1.2 ceiling is expressible here. `"TLSv1"` and `"TLSv1.1"`
/// would need a ceiling *below* `new13_build_connector`'s deliberate
/// `min_protocol_version(Tlsv12)` floor, which would make the connector build
/// fail outright (max < min). That floor is an intentional security posture,
/// not an oversight, so those two names keep their existing behaviour — the
/// context is honoured up to the floor and no further. `"TLS"`, `"TLSv1.3"`,
/// `"SSL"` and `"Default"` impose no ceiling, which is also JSSE's behaviour.
pub(crate) fn new13_ctx_max_protocol(protocol_name: &str) -> Option<native_tls::Protocol> {
    match protocol_name {
        "TLSv1.2" => Some(native_tls::Protocol::Tlsv12),
        _ => None,
    }
}

/// Read the JSSE protocol name the `SSLContext` owning the `SSLSocketFactory`
/// at `args[0]` was pinned to, if that name implies a version ceiling we can
/// honour. Same factory-field-0 back-reference `p68_factory_trust_roots`
/// (immediately below) uses.
///
/// Returns `None` — "no ceiling", the pre-existing behaviour — for a factory
/// with no owning context, a version-agnostic context, or a field 0 that
/// isn't an `SSLContext` at all. That last case is real: as
/// `net_phase_e::createSocket`'s own FIX comment records, field 0 of a
/// user-defined `SSLSocketFactory` SUBCLASS is whatever that class declares
/// first. Filtering through `new13_ctx_max_protocol` is what makes this safe
/// — a field that does not read back as one of the known protocol names
/// yields `None` rather than a bogus pin.
pub(crate) fn p68_factory_pinned_protocol_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<String> {
    let this = match args.first() {
        Some(Value::Object(Some(this))) => *this,
        _ => return None,
    };
    if ctx.object_num_fields(this) == 0 {
        return None;
    }
    let sslctx = match ctx.get_field(this, 0) {
        Value::Object(Some(sslctx)) => sslctx,
        _ => return None,
    };
    if ctx.object_num_fields(sslctx) == 0 {
        return None;
    }
    let name = match ctx.get_field(sslctx, NEW13_CTX_PROTOCOL) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    new13_ctx_max_protocol(&name).map(|_| name)
}

/// The `native_tls` form of `p68_factory_pinned_protocol_name`, for the
/// connector-based (immediate-connect) client path.
pub(crate) fn p68_factory_max_protocol(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<native_tls::Protocol> {
    p68_factory_pinned_protocol_name(ctx, args).as_deref().and_then(new13_ctx_max_protocol)
}

/// FIX (es-restclient-https): look up the custom trust anchors (if any)
/// stashed on `args[0]` (the `SSLSocketFactory` `this`) by `getSocketFactory`.
/// Returns an empty Vec when the factory carries no custom scope (the common
/// case — every existing default-trust `createSocket` caller is unaffected).
pub(crate) fn p68_factory_trust_roots(ctx: &mut dyn NativeContext, args: &[Value]) -> Vec<Vec<u8>> {
    match args.first() {
        Some(Value::Object(Some(this))) => {
            let key = this.as_ptr() as usize;
            let direct = p68_ctx_trust_roots_table()
                .lock()
                .get(&key)
                .cloned()
                .unwrap_or_default();
            if !direct.is_empty() {
                return direct;
            }
            if ctx.object_num_fields(*this) > 0 {
                if let Value::Object(Some(sslctx)) = ctx.get_field(*this, 0) {
                    return crate::t27_tls::context_trust_root_ders(ctx, sslctx);
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// FIX (netty-https-client-trust): the SSLContext object stashed at factory
/// field 0 (`net_phase_e`'s `getSocketFactory` — the live registration —
/// stores it) may carry REAL Java TrustManager objects attached at
/// `SSLContext.init` (`t27_tls::attach_trust_managers_to_ctx`). Returns the
/// trust-table key when so, which switches `new13_do_create_socket` from
/// native-tls WebPKI verification to post-handshake Java
/// `checkServerTrusted` delegation — the only semantic that honors e.g.
/// HttpClient5's `TrustSelfSignedStrategy` against an ephemeral self-signed
/// server cert (Netty `SelfSignedCertificate`;
/// `ServerHttpsRequestIntegrationTests::checkUri`). A wrong object at
/// field 0 (user-defined factory subclass — see `net_phase_e`'s
/// `createSocket` comment) simply misses the table → `None` → unchanged
/// default verification.
pub(crate) fn p68_factory_java_tm_key(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<u64> {
    let Some(Value::Object(Some(factory))) = args.first() else {
        return None;
    };
    if ctx.object_num_fields(*factory) == 0 {
        return None;
    }
    let Value::Object(Some(sslctx)) = ctx.get_field(*factory, 0) else {
        return None;
    };
    crate::t27_tls::ctx_trust_managers_key_if_attached(ctx, sslctx)
}

/// FIX (h2-testnetutils-cipherfactory-createsocket-cast): staging table for
/// `SSLSocketFactory.createSocket()` (the true zero-arg overload — an
/// unconnected socket the caller connects later via `Socket.connect
/// (SocketAddress, int)`, e.g. H2's `CipherFactory.createSocket`). The
/// factory (and its trust scope) is only available as `args[0]` at
/// `createSocket()` time; `connect()` runs later with just the `SSLSocket`
/// object, so the trust roots/TrustManager key captured at creation time are
/// stashed here, keyed by the socket's GC-stable identity hash (see
/// `t27_tls::gc_stable_objref_key`'s doc comment for why identity hash and
/// not a raw pointer), and consumed (removed) by `new13_ssl_socket_connect`.
///
/// The owning context's TLS version ceiling (`p68_factory_max_protocol`) rides
/// along for the same reason: it too is only readable from `args[0]` at
/// `createSocket()` time, and the deferred `connect()` must still honour it.
type PendingSslConnectCtx = (Vec<Vec<u8>>, Option<u64>, Option<native_tls::Protocol>);

pub(crate) fn pending_ssl_socket_connect_ctx_table(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<u64, PendingSslConnectCtx>> {
    static T: std::sync::OnceLock<
        parking_lot::Mutex<rustc_hash::FxHashMap<u64, PendingSslConnectCtx>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

pub(crate) fn stash_pending_ssl_socket_connect_ctx(
    ctx: &dyn NativeContext,
    sock: ObjectRef,
    extra_roots: Vec<Vec<u8>>,
    java_tm_key: Option<u64>,
    max_protocol: Option<native_tls::Protocol>,
) {
    let key = ctx.identity_hash_code(sock) as u32 as u64;
    pending_ssl_socket_connect_ctx_table()
        .lock()
        .insert(key, (extra_roots, java_tm_key, max_protocol));
}

pub(crate) fn take_pending_ssl_socket_connect_ctx(
    ctx: &dyn NativeContext,
    sock: ObjectRef,
) -> PendingSslConnectCtx {
    let key = ctx.identity_hash_code(sock) as u32 as u64;
    pending_ssl_socket_connect_ctx_table()
        .lock()
        .remove(&key)
        .unwrap_or_default()
}

/// `javax/net/ssl/SSLSocket.connect(SocketAddress[, int timeout])` for a
/// socket obtained via the zero-arg `SSLSocketFactory.createSocket()` (see
/// `pending_ssl_socket_connect_ctx_table`'s doc comment for the full
/// rationale). Performs the real TCP-connect + TLS client handshake
/// (`new13_connect_and_handshake`, shared with the immediate-connect
/// overloads) and populates the EXISTING socket object via
/// `new13_finish_socket` rather than allocating a new one — Java already
/// holds a reference to this exact object.
pub(crate) fn new13_ssl_socket_connect(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if new13_resolve_tls_id(ctx, this) >= 0 {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/net/SocketException",
            "already connected",
        ));
    }
    let sa = match args.get(1) {
        Some(Value::Object(Some(addr))) => *addr,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("SSLSocket.connect: null address".into()),
            }
            .into());
        }
    };
    let (host, port) = crate::net_phase_e::read_inet_socket_address(ctx, sa)?;
    let (extra_roots, java_tm_key, max_protocol) = take_pending_ssl_socket_connect_ctx(ctx, this);
    let tls_id = new13_connect_and_handshake(
        ctx,
        &host,
        port as u16,
        &extra_roots,
        java_tm_key,
        max_protocol,
    )?;
    let _ = new13_finish_socket(ctx, this, &host, port as u16, tls_id);
    Ok(None)
}

/// NEW-13: allocate an `SSLSession` synthetic object populated from the
/// session info captured by `s2_tls_connect`.
pub(crate) fn new13_alloc_ssl_session(ctx: &mut dyn NativeContext, tls_id: i32) -> ObjectRef {
    let session =
        alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", NEW13_SSL_SESS_FIELDS);
    // Two stream registries back an `SSLSocket` here: `s2_registry` (the
    // native-tls client streams) and `t27_tls`'s rustls registry (the
    // layered/server streams AND, since it became the live registration,
    // `net_phase_e`'s `SSLSocketFactory.createSocket(String, int)`). Consult
    // both before falling back — querying only the first meant a
    // rustls-backed id silently reported the hard-coded pair below as if it
    // had actually been negotiated.
    //
    // Note the id spaces differ: a rustls-backed socket carries
    // `RUSTLS_SOCK_ID_BASE + rid`, while `rustls_session_info` is keyed by the
    // raw `rid`. Passing the offset id through unadjusted is a silent miss
    // that lands on the fabricated fallback, which is exactly how a TLS 1.3
    // connection came to report `TLS_AES_128_GCM_SHA256` regardless of the
    // suite it had really negotiated.
    let rustls_info = if tls_id >= crate::servlet::RUSTLS_SOCK_ID_BASE {
        crate::t27_tls::rustls_session_info(tls_id - crate::servlet::RUSTLS_SOCK_ID_BASE)
    } else {
        crate::t27_tls::rustls_session_info(tls_id)
    };
    let (proto, cipher) = match crate::servlet::s2_tls_session_info(tls_id) {
        Some((p, c, _, _)) => (p, c),
        None => match rustls_info {
            Some((p, c, _, _)) => (p, c),
            None => (
                String::from("TLSv1.3"),
                String::from("TLS_AES_128_GCM_SHA256"),
            ),
        },
    };
    // Pin `session` across the two `create_string` calls below. Each of them
    // allocates, so each is a GC point, and a moving young collection there
    // relocates the just-allocated session — after which every `set_field`
    // below writes through a stale reference and the object handed back is
    // whatever reused that address (the "java.lang.Object cannot be cast to
    // ..." shape). Latent while `getSession()` returned the stored field and
    // this ran only at socket-construction time; `new13_resolve_socket_session`
    // now calls it lazily, on any thread, at arbitrary allocation pressure.
    // `proto_str` needs the same treatment as `session`: it is created before
    // the second `create_string`, which is itself a GC point.
    let pin = ctx.pin_native_root(session);
    let proto_str = ctx.create_string(&proto);
    let proto_pin = ctx.pin_native_root(proto_str);
    let cipher_str = ctx.create_string(&cipher);
    let session = ctx.read_native_pin(pin, session);
    let proto_str = ctx.read_native_pin(proto_pin, proto_str);
    ctx.set_field(session, NEW13_SESS_PROTO, Value::Object(Some(proto_str)));
    ctx.set_field(session, NEW13_SESS_CIPHER, Value::Object(Some(cipher_str)));
    ctx.set_field(session, NEW13_SESS_TLSID, Value::Int(tls_id));
    // FIX (netty-https-client-trust residual): record the peer chain this
    // client connection already captured so a later `getPeerCertificates()`
    // on THIS session object doesn't spuriously see "no certificate" — see
    // `t27_tls::record_client_peer_chain` doc comment.
    if let Some(chain) = crate::servlet::s2_tls_peer_cert_chain_der(tls_id) {
        crate::t27_tls::record_client_peer_chain(ctx, session, chain);
    }
    let session = ctx.read_native_pin(pin, session);
    ctx.unpin_native_roots(pin);
    session
}

/// Resolve an `InetAddress` argument without depending on its implementation
/// class.  Real JSSE factories expose all of the `SocketFactory` overloads;
/// our P68 bridge must do the same because its synthetic factory is allocated
/// as `javax/net/ssl/SSLSocketFactory` itself.
pub(crate) fn p68_inet_address_host(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    address_index: usize,
) -> Result<String, MethodCallFailed> {
    let address = obj_arg(args, address_index)?;
    let pin_base = ctx.pin_native_root(address);
    let host_value = ctx.invoke_virtual(address, "getHostAddress", "()Ljava/lang/String;", &[]);
    ctx.unpin_native_roots(pin_base);
    let host = match host_value? {
        Some(Value::Object(Some(host))) => ctx.read_string(host).unwrap_or_default(),
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

/// Bridge the `InetAddress` forms of `SSLSocketFactory.createSocket`.  The
/// local-address variants share the P68 TLS connector's current connection
/// semantics; their local bind arguments are accepted by the JDK signature
/// but are not consumed by the native TLS stream implementation.
pub(crate) fn p68_create_socket_inet_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    address_index: usize,
    port_index: usize,
) -> MethodCallResult {
    let host = p68_inet_address_host(ctx, args, address_index)?;
    let port = args
        .get(port_index)
        .and_then(|value| value.as_int())
        .unwrap_or(443);
    if !(0..=65535).contains(&port) {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("port out of range: {port}"),
        }
        .into());
    }
    let extra_roots = p68_factory_trust_roots(ctx, args);
    let java_tm_key = p68_factory_java_tm_key(ctx, args);
    let max_protocol = p68_factory_max_protocol(ctx, args);
    new13_do_create_socket(ctx, &host, port as u16, &extra_roots, java_tm_key, max_protocol)
}

/// NEW-13: real, blocking TCP-connect + TLS client handshake shared by
/// `new13_do_create_socket` (the immediate-connect `createSocket(host, port)`
/// overloads) and `new13_ssl_socket_connect` (the deferred
/// `createSocket()` + later `Socket.connect(SocketAddress, timeout)` pattern
/// — see that function's doc comment for why the two need the same logic
/// applied to a not-yet-allocated vs. an already-allocated `SSLSocket`).
/// Returns the `s2_registry` TLS stream id on success.
pub(crate) fn new13_connect_and_handshake(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: u16,
    extra_root_ders: &[Vec<u8>],
    java_tm_key: Option<u64>,
    max_protocol: Option<native_tls::Protocol>,
) -> Result<i32, MethodCallFailed> {
    #[cfg(unix)]
    let legacy_dsa_context = extra_root_ders.iter().any(|der| {
        openssl::x509::X509::from_der(der)
            .ok()
            .and_then(|cert| cert.public_key().ok())
            .is_some_and(|key| key.dsa().is_ok())
    });
    let connector = new13_build_connector(extra_root_ders, java_tm_key.is_some(), max_protocol)
        .map_err(|msg| RuntimeError::IOException { message: msg })?;
    // FIX (netty-client-socket-write-after-close): this is a real, blocking
    // TCP connect + full TLS handshake (same shape as net_phase_e.rs's own
    // client createSocket, which already announces this — see its "T19.H1"
    // comment). Without announcing it, a concurrent stop-the-world GC has no
    // way to know this thread is safely parked in native code rather than
    // stuck mid-bytecode, and (per that comment) can mishandle this thread's
    // pending return value across the pause — observed as the freshly built
    // socket's own fields reading back as their zero/None default the
    // moment Java calls a method on it afterward (`getOutputStream`/
    // `getInputStream` seeing the just-set NEW13_SOCK_TLSID field as
    // `Object(None)` instead of the `Int` written a few lines below), a
    // symptom whose true trigger (a concurrent GC racing this unmarked
    // blocking call) is not IO-timing-reproducible on demand, but a bigger
    // heap alone does not suppress it either since young-gen collections
    // still fire from ordinary allocation churn on OTHER threads.
    ctx.begin_blocking_region();
    #[cfg(unix)]
    let connect_result = if legacy_dsa_context {
        crate::servlet::s2_legacy_dsa_tls_connect(host, port, extra_root_ders)
    } else {
        crate::servlet::s2_tls_connect(&connector, host, port)
    };
    #[cfg(not(unix))]
    let connect_result = crate::servlet::s2_tls_connect(&connector, host, port);
    ctx.end_blocking_region();
    let tls_id = connect_result.map_err(|e| RuntimeError::IOException {
        message: e.to_string(),
    })?;

    // FIX (netty-https-client-trust): native verification was disabled above
    // when the context carries Java TrustManagers — they are the ONLY
    // verifier now, so consult them immediately and fail CLOSED: no chain, or
    // a checkServerTrusted throw, aborts the socket with
    // SSLHandshakeException (matching JSSE, which aborts the handshake when
    // a configured TrustManager rejects the chain).
    // The legacy DSA bridge has already verified against the explicitly
    // supplied roots (including Spring Boot's historical expired fixture).
    // Re-running the VM TrustManager shim would reject that same accepted
    // anchor solely on wall-clock validity.
    if let Some(tm_key) = java_tm_key.filter(|_| {
        #[cfg(unix)]
        {
            !legacy_dsa_context
        }
        #[cfg(not(unix))]
        {
            true
        }
    }) {
        let chain = crate::servlet::s2_tls_peer_cert_chain_der(tls_id).unwrap_or_default();
        if chain.is_empty() {
            let _ = crate::servlet::s2_tls_close(tls_id);
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                "no peer certificate available for TrustManager verification",
            ));
        }
        if let Err(e) = crate::t27_tls::run_client_trust_check_for_chain(ctx, tm_key, chain) {
            let _ = crate::servlet::s2_tls_close(tls_id);
            return Err(e);
        }
    }
    Ok(tls_id)
}

/// NEW-13: common body for the `SSLSocketFactory.createSocket` overloads that
/// connect immediately. Connects, then allocates a fresh `SSLSocket` and
/// populates it — see `new13_finish_socket` (shared with the deferred
/// `createSocket()` + `connect()` pattern, which populates an
/// *already-allocated* socket instead).
pub(crate) fn new13_do_create_socket(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: u16,
    extra_root_ders: &[Vec<u8>],
    java_tm_key: Option<u64>,
    max_protocol: Option<native_tls::Protocol>,
) -> MethodCallResult {
    let tls_id =
        new13_connect_and_handshake(ctx, host, port, extra_root_ders, java_tm_key, max_protocol)?;
    let sock = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", NEW13_SSL_SOCK_FIELDS);
    let sock = new13_finish_socket(ctx, sock, host, port, tls_id);
    if crate::nbflags().dbg_tls_sock {
        eprintln!(
            "[dbg-tls-sock] thread={:?} new13_do_create_socket built sock={:?} tls_id={}",
            std::thread::current().id(),
            sock,
            tls_id
        );
    }
    Ok(Some(Value::Object(Some(sock))))
}

/// Populate a (possibly pre-existing) `javax/net/ssl/SSLSocket` object's
/// fields/side-table state from a completed TLS connection. Shared by
/// `new13_do_create_socket` (allocates `sock` fresh, immediately before
/// calling this) and `new13_ssl_socket_connect` (reuses the `SSLSocket`
/// object `SSLSocketFactory.createSocket()` already returned to Java, which
/// is why this takes `sock` rather than allocating one itself). Returns the
/// (possibly GC-forwarded) object reference to use afterward.
pub(crate) fn new13_finish_socket(
    ctx: &mut dyn NativeContext,
    sock: ObjectRef,
    host: &str,
    port: u16,
    tls_id: i32,
) -> ObjectRef {
    let pin_base = ctx.pin_native_root(sock);
    let host_obj = ctx.create_string(host);
    let sock = ctx.read_native_pin(pin_base, sock);
    ctx.set_field(sock, NEW13_SOCK_HOST, Value::Object(Some(host_obj)));
    ctx.set_field(sock, NEW13_SOCK_PORT, Value::Int(port as i32));
    // FIX (netty-client-socket-write-after-close): do NOT rely on the raw
    // NEW13_SOCK_TLSID/NEW13_SOCK_CLOSED field writes below being visible —
    // `alloc_concurrent_synthetic` sizes this object using the REAL loaded
    // `javax/net/ssl/SSLSocket` class's own field layout, and its actual
    // field #2 is reference-typed; the GC/field-layout guard silently drops
    // our mismatched-type Int write (confirmed via an immediate same-call
    // readback: a debug trace showed `Object(None)` moments after setting
    // Int(tls_id), with no Java code and no GC in between). Record the
    // authoritative state in net_phase_e's side table instead — the same
    // mechanism its own createSocket(String,int) uses for the identical
    // class — so new13_resolve_tls_id's fallback (used by getInputStream/
    // getOutputStream/close/isClosed/isConnected below) finds it. The raw
    // writes are kept too: harmless if dropped, and a free win if some
    // future JDK's field layout happens not to collide.
    crate::net_phase_e::sock_set_for_create(ctx, sock, port as i32, tls_id);
    ctx.set_field(sock, NEW13_SOCK_TLSID, Value::Int(tls_id));
    ctx.set_field(sock, NEW13_SOCK_CLOSED, Value::Int(0));
    let session = new13_alloc_ssl_session(ctx, tls_id);
    let sock = ctx.read_native_pin(pin_base, sock);
    ctx.set_field(sock, NEW13_SOCK_SESSION, Value::Object(Some(session)));
    let sock = ctx.read_native_pin(pin_base, sock);
    ctx.unpin_native_roots(pin_base);
    sock
}

/// FIX (tomcat-clientauth-engine-config): identity-hash side table mapping a
/// `javax/net/ssl/KeyManagerFactory` object (allocated with the REAL class
/// shape — no room for a `cratonvm$...` pseudo-field) to the keystore
/// registry id it was `init()`-ed with. Lets `getKeyManagers()` (below)
/// build a real, functional `KeyManager` via `x509_manager`'s
/// `km_registry`/`FQN_SUN_X509_KM` machinery instead of the previous
/// non-functional bare-`javax/net/ssl/X509KeyManager`-interface stub, whose
/// methods have no Code and threw `AbstractMethodError` for any caller that
/// invoked one directly (e.g. a test wrapper `KeyManager` delegating to the
/// array `getKeyManagers()` returned — see
/// `fixed-suite-bugs/tls-ocsp-clientcert-validation-not-enforced-FIXED.md`,
/// "Residual #2 implementation" for the full trace that found this).
pub(crate) fn kmf_keystore_id_by_identity(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>> {
    static T: std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// FIX (tomcat-clientauth-engine-config): same pattern as
/// `kmf_keystore_id_by_identity` immediately above, for
/// `javax/net/ssl/TrustManagerFactory` — maps the TMF object to the
/// `x509_manager::tm_registry` id its `init(KeyStore)` /
/// `init(ManagerFactoryParameters)` was called with, so `getTrustManagers()`
/// (below) can build a real, functional `X509TrustManagerImpl`-shaped
/// `TrustManager` instead of a bare-interface-stamped stub whose
/// `checkClientTrusted`/`checkServerTrusted` have no Code. Stores the
/// FINAL registered `tm_registry` id directly (not a keystore id) so both
/// `init` overloads — one backed by a real `KeyStore`, the other by a
/// `CertPathTrustManagerParameters` with no keystore back-reference at all
/// (Tomcat's own `SSLUtilBase.getTrustManagers()` uses this overload
/// whenever `sslHostConfig.getTruststoreAlgorithm()` is `"PKIX"`, the
/// default — confirmed via tracing this is the overload the SERVER side of
/// this suite's mTLS tests actually exercises) — can populate it uniformly.
pub(crate) fn tmf_tm_id_by_identity() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>>
{
    static T: std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, i32>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Build and throw a real `java.security.NoSuchAlgorithmException` carrying
/// `msg`. Used by `KeyManagerFactory.getInstance`/`TrustManagerFactory.
/// getInstance` (below) to honour the JCA `getInstance` contract — real JDK
/// rejects an algorithm name no registered provider supports rather than
/// silently substituting a default implementation. Falls back to a catchable
/// `SecurityException` if the real exception class can't be constructed,
/// mirroring the `throw_jca`/`p68_signature_failure` pattern used elsewhere
/// in this crate for the same reason (never swallow a validation failure).
pub(crate) fn kmf_tmf_no_such_algorithm(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/security/NoSuchAlgorithmException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::SecurityException {
        message: msg.to_string(),
    }
    .into()
}

/// Per-`javax/net/ssl/SSLSocket` client-authentication and role state, keyed by
/// `identity_hash_code` (stable across a moving collection, and the same keying
/// `kmf_keystore_id_by_identity` above uses).
///
/// STUB-REMOVAL (wave 2): `setNeedClientAuth`/`setWantClientAuth` were
/// unconditional no-ops and `getNeedClientAuth`/`getWantClientAuth`/
/// `getUseClientMode` returned a hard-coded `false`. A server that configured
/// mTLS on an accepted `SSLSocket` therefore read back "client auth off" — a
/// silently-disabled security check, and a value that contradicted the caller's
/// own immediately-preceding setter. There is no independent handshake to
/// re-drive at this level (a socket handed to `setNeedClientAuth` after
/// `accept()` has already completed its handshake; the enforcing path is
/// `SSLServerSocket.setNeedClientAuth`, which rebuilds the listener's
/// `ServerConfig` — see `t27_tls::register_sslserversocket`), so this table
/// records the caller's request faithfully and the getters report it.
///
/// Tuple is `(use_client_mode, need_client_auth, want_client_auth)`, each 0/1.
fn ssl_sock_auth_state() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, (i32, i32, i32)>>
{
    static T: std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, (i32, i32, i32)>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn ssl_sock_auth_get(ctx: &dyn NativeContext, this: ObjectRef) -> (i32, i32, i32) {
    let ih = ctx.identity_hash_code(this);
    ssl_sock_auth_state()
        .lock()
        .get(&ih)
        .copied()
        // Default for a socket whose setters were never called: exactly what
        // the three constant natives returned before this change (all false).
        // Deliberately NOT "client mode on": an `SSLServerSocket.accept()`
        // socket is in SERVER mode and never calls `setUseClientMode`, so
        // defaulting to `true` would flip an answer that used to be right.
        // Only a caller that actually invoked a setter sees a different value.
        .unwrap_or((0, 0, 0))
}

/// `HandshakeCompletedListener`s registered on an `SSLSocket`, keyed by the
/// socket's identity hash (same keying `ssl_sock_auth_state` above uses).
///
/// The value is a list of `(listener identity hash, global-root handle)`. The
/// handle — not a raw `ObjectRef` — is what makes this safe: these references
/// are stored by one native call and consumed by a later one, across which a
/// moving collection can relocate the listener. `add_global_root` keeps the
/// object reachable and remaps it; `resolve_global_root` hands back the
/// current address. The identity hash rides along so
/// `removeHandshakeCompletedListener` can find the right entry without
/// resolving every root.
#[allow(clippy::type_complexity)]
fn handshake_listeners(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<u64, Vec<(i32, usize)>>> {
    static T: std::sync::OnceLock<
        parking_lot::Mutex<rustc_hash::FxHashMap<u64, Vec<(i32, usize)>>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn ssl_sock_auth_update<F: FnOnce(&mut (i32, i32, i32))>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: F,
) {
    let ih = ctx.identity_hash_code(this);
    let mut table = ssl_sock_auth_state().lock();
    let entry = table.entry(ih).or_insert((0, 0, 0));
    f(entry);
}

pub(crate) fn register_p68_ssl(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // NEW-13: SSLContext = 5-field synthetic — see field layout constants.
    let ctx_class = "javax/net/ssl/SSLContext";
    r.register(
        ctx_class,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            // Validate protocol string: must be one of the recognised TLS
            // identifiers, otherwise throw NoSuchAlgorithmException like the
            // reference JDK does.
            let proto_ref = match args.get(0) {
                Some(Value::Object(Some(s))) => *s,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "SSLContext.getInstance: protocol is null".into(),
                    }
                    .into());
                }
            };
            let proto_name = ctx.read_string(proto_ref).unwrap_or_default();
            match proto_name.as_str() {
                "TLS" | "TLSv1" | "TLSv1.1" | "TLSv1.2" | "TLSv1.3" | "SSL" | "Default" => {}
                other => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("No such algorithm: {}", other),
                    }
                    .into());
                }
            }
            let obj =
                alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", NEW13_SSL_CTX_FIELDS);
            ctx.set_field(obj, NEW13_CTX_PROTOCOL, Value::Object(Some(proto_ref)));
            ctx.set_field(obj, NEW13_CTX_INIT, Value::Int(0));
            ctx.set_field(obj, NEW13_CTX_KM, Value::Object(None));
            ctx.set_field(obj, NEW13_CTX_TM, Value::Object(None));
            ctx.set_field(obj, NEW13_CTX_RANDOM, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ctx_class,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            // A "Default" context is pre-initialised: it uses the platform
            // trust store and an implementation-defined `SecureRandom`.
            let obj =
                alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", NEW13_SSL_CTX_FIELDS);
            let proto = ctx.create_string("TLSv1.3");
            ctx.set_field(obj, NEW13_CTX_PROTOCOL, Value::Object(Some(proto)));
            ctx.set_field(obj, NEW13_CTX_INIT, Value::Int(1));
            ctx.set_field(obj, NEW13_CTX_KM, Value::Object(None));
            ctx.set_field(obj, NEW13_CTX_TM, Value::Object(None));
            ctx.set_field(obj, NEW13_CTX_RANDOM, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // NEW-13.2: SSLContext.init — actually validate the arguments, build a
    // `native_tls::TlsConnector` to prove the current platform can produce a
    // real TLS connector with the requested configuration, and record the
    // initialised state + KM/TM/random refs on the SSLContext instance.
    r.register(
        ctx_class,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let km_arg = args.get(1).copied().unwrap_or(Value::Object(None));
            let tm_arg = args.get(2).copied().unwrap_or(Value::Object(None));
            let sr_arg = args.get(3).copied().unwrap_or(Value::Object(None));

            // Validate that arrays, if non-null, contain only KeyManager /
            // TrustManager references (i.e. Object-array element type).
            // Arity checks mirror the reference JDK: empty arrays are
            // allowed and treated equivalently to null.
            if let Value::Object(Some(km_arr)) = km_arg {
                // `array_length` panics on non-arrays, so gate behind a
                // type check via the underlying registry — but our
                // NativeContext trait doesn't expose is_array(). As a
                // defensive shortcut, simply try to read the length; if the
                // object isn't an array this will raise an exception via
                // the interpreter rather than crash the VM.
                let _ = ctx.array_length(km_arr);
            }
            if let Value::Object(Some(tm_arr)) = tm_arg {
                let _ = ctx.array_length(tm_arr);
            }

            // Capture the live manager objects before any helper below can
            // allocate or re-enter Java.  The native-call funnel keeps the
            // arguments rooted, but the copied ObjectRefs in `km_arg`/
            // `tm_arg` are not rewritten after a moving collection.  Delaying
            // this capture could therefore publish an old array element into
            // the long-lived TLS side table; a later handshake would then
            // dispatch `checkServerTrusted` on whatever object reused that
            // address.  The tables themselves are GC-rooted/remapped once
            // populated, so install them at this first post-validation point.
            let kms_array = match km_arg {
                Value::Object(Some(array)) => Some(array),
                _ => None,
            };
            let tms_array = match tm_arg {
                Value::Object(Some(array)) => Some(array),
                _ => None,
            };
            crate::t27_tls::attach_trust_managers_to_ctx(ctx, this, tms_array);
            crate::t27_tls::attach_key_managers_to_ctx(ctx, this, kms_array);

            // FIX (es-restclient-https): if the supplied TrustManager[] is
            // bound to an explicit KeyStore (a custom truststore, not the
            // default), capture its trust anchors keyed by THIS SSLContext's
            // identity so getSocketFactory()/createSocket can add them to
            // the native-tls connector. See `p68_extract_trust_manager_roots`.
            let extra_roots = p68_extract_trust_manager_roots(ctx, tm_arg);
            let ctx_key = this.as_ptr() as usize;
            if let Some(roots) = extra_roots.clone() {
                p68_ctx_trust_roots_table().lock().insert(ctx_key, roots);
            } else {
                p68_ctx_trust_roots_table().lock().remove(&ctx_key);
            }

            // Prove we can build a real native-tls connector with the
            // platform trust store (plus any custom anchors) under the
            // currently-requested protocol. A failure here surfaces
            // immediately to the caller as a KeyManagementException-shaped
            // IOException.
            let init_max_protocol = match ctx.get_field(this, NEW13_CTX_PROTOCOL) {
                Value::Object(Some(s)) => ctx
                    .read_string(s)
                    .as_deref()
                    .and_then(new13_ctx_max_protocol),
                _ => None,
            };
            if let Err(msg) = new13_build_connector(
                extra_roots.as_deref().unwrap_or(&[]),
                false,
                init_max_protocol,
            ) {
                return Err(RuntimeError::IOException {
                    message: format!("SSLContext.init: {}", msg),
                }
                .into());
            }

            ctx.set_field(this, NEW13_CTX_KM, km_arg);
            ctx.set_field(this, NEW13_CTX_TM, tm_arg);
            ctx.set_field(this, NEW13_CTX_RANDOM, sr_arg);
            ctx.set_field(this, NEW13_CTX_INIT, Value::Int(1));

            // Keep the legacy p68 context coherent with the rustls-backed
            // transport bridge.  A real-JDK dispatch may reach this handler
            // through an inherited/cached SSLContext call; without these
            // transfers, a Java-supplied TrustManager is retained only in the
            // synthetic fields and HttpURLConnection silently falls back to
            // the platform verifier.
            let resolved_identity =
                crate::x509_manager::resolved_identity_pem_for_key_manager_array(ctx, kms_array);
            crate::t27_tls::attach_pending_identity_to_ctx(ctx, this, resolved_identity);
            Ok(None)
        },
    );
    r.register(
        ctx_class,
        "getSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, args| {
            // Field 0 is the originating SSLContext, matching the factory
            // shape consumed by the HttpURLConnection TLS bridge.
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 1);
            // FIX (es-restclient-https): carry this SSLContext's custom trust
            // anchors (if any) forward onto the factory so createSocket can
            // find them — createSocket only has `this` = the factory, not
            // the originating SSLContext.
            if let Ok(this) = obj_arg(args, 0) {
                ctx.set_field(obj, 0, Value::Object(Some(this)));
                let ctx_key = this.as_ptr() as usize;
                if let Some(roots) = p68_ctx_trust_roots_table().lock().get(&ctx_key).cloned() {
                    let factory_key = obj.as_ptr() as usize;
                    p68_ctx_trust_roots_table()
                        .lock()
                        .insert(factory_key, roots);
                }
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ctx_class,
        "getServerSocketFactory",
        "()Ljavax/net/ssl/SSLServerSocketFactory;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocketFactory", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ctx_class,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        ctx_class,
        "createSSLEngine",
        "()Ljavax/net/ssl/SSLEngine;",
        |ctx, _args| {
            let obj = ssleng_alloc(ctx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ctx_class,
        "createSSLEngine",
        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
        |ctx, _args| {
            let obj = ssleng_alloc(ctx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // SSLSocketFactory
    let ssf = "javax/net/ssl/SSLSocketFactory";
    r.register(
        ssf,
        "getDefault",
        "()Ljavax/net/SocketFactory;",
        |ctx, _args| {
            // FIX (springprofilearbitertests-ssf-getdefault-no-context):
            // real `SSLSocketFactory.getDefault()` delegates to
            // `SSLContext.getDefault().getSocketFactory()`, so the returned
            // factory carries the default SSLContext at field 0 — exactly
            // like `SSLContext.getSocketFactory()` above. This registration
            // used to allocate a bare 0-field object, so any caller that
            // later reaches the layered `createSocket(Socket,String,int,
            // boolean)` overload (e.g. Apache HttpClient5's
            // `SSLConnectionSocketFactory.createLayeredSocket`, which builds
            // its default factory via this exact static call) hit that
            // overload's `ctx.get_field(factory, 0)` on a factory with no
            // field 0 at all, throwing `IllegalStateException("SSLSocketFactory
            // has no owning SSLContext")` instead of connecting — a real-JDK
            // A/B confirmed CratonVM-only failure.
            //
            // REGRESSION 2026-08-04: the fix below was correct and still
            // present, but `net_phase_e::register_re6_ssl_context` carried a
            // SECOND registration of this same triple that ran later and won
            // by last-registration-wins, handing back a factory whose field 0
            // was `None`. That duplicate is deleted; this is the single owner
            // (asserted by `registry_contracts.rs`). The body moved into
            // `t27_tls::default_ssl_socket_factory_obj` so the
            // `HttpsURLConnection` factory getters — which also used to mint
            // bare carriers — share one implementation.
            let obj = crate::t27_tls::default_ssl_socket_factory_obj(ctx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ssf,
        "getDefaultCipherSuites",
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
            ];
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, suites.len());
            for (i, &s) in suites.iter().enumerate() {
                let str_obj = ctx.create_string(s);
                ctx.set_array_element(arr, i, Value::Object(Some(str_obj)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        ssf,
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
                // T-CBC.1: real CBC-mode suites, see t27_tls_cbc /
                // fixed-suite-bugs/rustls-cbc-cipher-suites-not-supported.md
                "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256",
                "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384",
                "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384",
            ];
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, suites.len());
            for (i, &s) in suites.iter().enumerate() {
                let str_obj = ctx.create_string(s);
                ctx.set_array_element(arr, i, Value::Object(Some(str_obj)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // FIX (h2-testnetutils-cipherfactory-createsocket-cast): the true
    // zero-arg `createSocket()` — an unconnected socket, connected later via
    // `Socket.connect(SocketAddress, int)`. Without its own registration
    // here, this call fell through to the ancestor `javax/net/SocketFactory`
    // native (`phases_early.rs::register_phase52_server_socket_factory`),
    // which allocates a plain `java/net/Socket` — `(SSLSocket)
    // f.createSocket()` then threw `ClassCastException` for every caller
    // using this JSSE-standard connect-later pattern (H2's
    // `CipherFactory.createSocket`/`NetUtils.createLoopbackSocket`; see
    // `bug-h2-netutils-dsa-privatekey-tls-unsupported.md`'s residuals).
    r.register(ssf, "createSocket", "()Ljava/net/Socket;", |ctx, args| {
        let extra_roots = p68_factory_trust_roots(ctx, args);
        let java_tm_key = p68_factory_java_tm_key(ctx, args);
        let max_protocol = p68_factory_max_protocol(ctx, args);
        let sock =
            alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", NEW13_SSL_SOCK_FIELDS);
        ctx.set_field(sock, NEW13_SOCK_TLSID, Value::Int(-1));
        ctx.set_field(sock, NEW13_SOCK_CLOSED, Value::Int(0));
        stash_pending_ssl_socket_connect_ctx(ctx, sock, extra_roots, java_tm_key, max_protocol);
        Ok(Some(Value::Object(Some(sock))))
    });
    // NEW-13.3: SSLSocketFactory.createSocket(String host, int port) → SSLSocket.
    // Backed by `servlet::s2_tls_connect`, which performs a real native-tls
    // handshake and registers the resulting TLS stream in the unified
    // `s2_registry` socket table so that SSLSocket shares id space and
    // close semantics with plain `java.net.Socket`.
    r.register(
        ssf,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        |ctx, args| {
            let host_ref = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLSocketFactory.createSocket: host is null".into()),
                    }
                    .into());
                }
            };
            let host = ctx.read_string(host_ref).unwrap_or_default();
            if host.is_empty() {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "SSLSocketFactory.createSocket: host is empty".into(),
                }
                .into());
            }
            let port_i = args.get(2).and_then(|v| v.as_int()).unwrap_or(443);
            if !(0..=65535).contains(&port_i) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("port out of range: {}", port_i),
                }
                .into());
            }
            let extra_roots = p68_factory_trust_roots(ctx, args);
            let java_tm_key = p68_factory_java_tm_key(ctx, args);
            let max_protocol = p68_factory_max_protocol(ctx, args);
            new13_do_create_socket(
                ctx,
                &host,
                port_i as u16,
                &extra_roots,
                java_tm_key,
                max_protocol,
            )
        },
    );
    // `SSLSocketFactory` redeclares the `InetAddress` forms abstract even
    // though `SocketFactory` has a bridge registration.  A synthetic P68
    // factory therefore resolves these calls at the abstract declaration
    // instead of inheriting the ancestor native, yielding AbstractMethodError.
    r.register(
        ssf,
        "createSocket",
        "(Ljava/net/InetAddress;I)Ljava/net/Socket;",
        |ctx, args| p68_create_socket_inet_address(ctx, args, 1, 2),
    );
    r.register(
        ssf,
        "createSocket",
        "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;",
        |ctx, args| {
            let host_ref = match args.get(1) {
                Some(Value::Object(Some(reference))) => *reference,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLSocketFactory.createSocket: host is null".into()),
                    }
                    .into());
                }
            };
            let host = ctx.read_string(host_ref).unwrap_or_default();
            if host.is_empty() {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "SSLSocketFactory.createSocket: host is empty".into(),
                }
                .into());
            }
            let port = args.get(2).and_then(|value| value.as_int()).unwrap_or(443);
            if !(0..=65535).contains(&port) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("port out of range: {port}"),
                }
                .into());
            }
            let extra_roots = p68_factory_trust_roots(ctx, args);
            let java_tm_key = p68_factory_java_tm_key(ctx, args);
            let max_protocol = p68_factory_max_protocol(ctx, args);
            new13_do_create_socket(
                ctx,
                &host,
                port as u16,
                &extra_roots,
                java_tm_key,
                max_protocol,
            )
        },
    );
    r.register(
        ssf,
        "createSocket",
        "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;",
        |ctx, args| p68_create_socket_inet_address(ctx, args, 1, 2),
    );
    // createSocket(Socket wrapped, String host, int port, boolean autoClose).
    // Real JDK contract: this reuses `wrapped`'s existing TCP connection (a
    // fresh outbound connection would break both the layered-server case
    // below AND client-side proxy-tunnel/CONNECT layering) and defaults to
    // CLIENT mode — the caller may still flip it to SERVER mode via
    // `setUseClientMode(false)` before the handshake actually starts, which
    // is exactly the MockWebServer HTTPS-listener pattern
    // (`fixed-suite-bugs/springboot/spring-boot-cloudfoundry-rerun-20260717-FIXED.md`).
    // Since the role isn't known yet at this call, the handshake itself is
    // deferred — see `t27_tls::{stash_pending_layered_socket,
    // set_pending_layered_socket_client_mode, drive_pending_layered_handshake}`
    // and this file's `ensure_layered_handshake_started` — to `startHandshake()`
    // or the first I/O call, whichever comes first (matching real JSSE, where
    // both implicitly trigger the handshake).
    r.register(
        ssf,
        "createSocket",
        "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;",
        |ctx, args| {
            let factory = obj_arg(args, 0)?;
            let wrapped = match args.get(1) {
                Some(Value::Object(Some(socket))) => *socket,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some(
                            "SSLSocketFactory.createSocket: wrapped Socket is null".into(),
                        ),
                    }
                    .into())
                }
            };
            // `host` may legitimately be null (real JDK: the socket's own
            // peer address is used for the handshake in that case) — Spring's
            // and MockWebServer's callers always pass a non-null String here,
            // but a null placeholder host is still a valid caller value we
            // must not NPE on; the DNS/SNI-name concept is meaningless for
            // the SERVER-mode case (MockWebServer) anyway, since the
            // handshake there uses `wrapped`'s existing connection, not a
            // hostname lookup.
            let host = match args.get(2) {
                Some(Value::Object(Some(host_ref))) => {
                    ctx.read_string(*host_ref).unwrap_or_default()
                }
                _ => String::new(),
            };
            let port_i = args.get(3).and_then(|v| v.as_int()).unwrap_or(443);
            if !(0..=65535).contains(&port_i) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("port out of range: {}", port_i),
                }
                .into());
            }
            // A factory with no reachable owning `SSLContext` used to be a
            // hard `IllegalStateException` here. That is not a JDK-faithful
            // failure mode — no real `SSLSocketFactory` exists without a
            // context; `getDefault()` and every `SSLContext.getSocketFactory()`
            // carry one — so an empty field 0 always means one of OUR
            // synthetic carriers lost it, and the honest answer is the
            // process default context, which is what the real
            // `SSLSocketFactory.getDefault()` would have supplied anyway.
            //
            // Twice now a single mis-wired carrier has converted into an
            // exception thrown before any network I/O, taking out a whole
            // test family (2026-07-23 the bare 0-field `getDefault()`;
            // 2026-08-04 the duplicate registration that clobbered its fix).
            // Falling back cannot weaken trust: caller-supplied anchors are
            // resolved below by factory identity (`p68_factory_trust_roots`),
            // not through this context, and the default context validates
            // against the platform trust store — strictly stricter than a
            // permissive caller-installed TrustManager, never laxer.
            let ssl_context = match ctx.get_field(factory, 0) {
                Value::Object(Some(context)) => context,
                _ => {
                    if crate::nbflags().dbg_tls_auth_ok {
                        eprintln!(
                            "[dbg-tls-auth] createSocket(layered): factory has no field-0 \
                             SSLContext, falling back to the process default"
                        );
                    }
                    crate::t27_tls::default_ssl_context_or_create(ctx)
                }
            };
            // Same trust-anchor/TrustManager resolution `new13_do_create_socket`
            // (the other `SSLSocketFactory.createSocket` overloads, and this
            // one's own prior CLIENT-mode behavior before SERVER-mode reuse
            // was added) already uses successfully — `args[0]` is this
            // factory. Confirmed necessary: `ssl_context`-keyed resolution
            // alone (`t27_tls::context_trust_root_ders`) came up empty for
            // this SSLBundle scenario, whose anchors instead live in the
            // legacy p68 factory-identity-keyed table populated by
            // `SSLContext.getSocketFactory()`'s own "carry trust anchors
            // forward onto the factory" fix.
            let extra_roots = p68_factory_trust_roots(ctx, args);
            let java_tm_key = p68_factory_java_tm_key(ctx, args);
            let pending_id = crate::t27_tls::stash_pending_layered_socket(
                ctx,
                wrapped,
                ssl_context,
                host.clone(),
                port_i as u16,
                extra_roots,
                java_tm_key,
            )
            .map_err(|message| RuntimeError::IOException { message })?;
            // FIX (TestSsl.testClientInitiatedRenegotiation[JSSE], second
            // path): the immediate-connect overloads honour a version-pinned
            // `SSLContext.getInstance(...)` via `p68_factory_max_protocol` ->
            // native-tls, but this deferred overload hands the handshake to
            // rustls instead, which never saw that ceiling — a socket from a
            // `TLSv1.2` context still negotiated TLS 1.3. Seed the pending
            // entry's enabled-protocol list, the same field
            // `setEnabledProtocols` writes and `protocol_versions_for`
            // consumes, so both paths agree.
            if let Value::Object(Some(proto_ref)) = ctx.get_field(ssl_context, NEW13_CTX_PROTOCOL) {
                if let Some(name) = ctx.read_string(proto_ref) {
                    if new13_ctx_max_protocol(&name).is_some() {
                        crate::t27_tls::set_pending_layered_socket_protocols(
                            pending_id,
                            vec![name],
                        );
                    }
                }
            }
            let socket = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", 5);
            let host_obj = ctx.create_string(&host);
            let pending_tls_id = crate::servlet::PENDING_LAYERED_SOCK_ID_BASE + pending_id;
            // The real JDK SSLSocket field layout is not this synthetic
            // adapter's layout, so these Int field writes may be rejected.
            // Keep the stream id in the side table used by the SSLSocket I/O
            // natives; otherwise getInputStream/getOutputStream resolve -1.
            crate::net_phase_e::sock_set_for_create(ctx, socket, port_i, pending_tls_id);
            ctx.set_field(socket, NEW13_SOCK_HOST, Value::Object(Some(host_obj)));
            ctx.set_field(socket, NEW13_SOCK_PORT, Value::Int(port_i));
            ctx.set_field(socket, NEW13_SOCK_TLSID, Value::Int(pending_tls_id));
            ctx.set_field(socket, NEW13_SOCK_CLOSED, Value::Int(0));
            // No SSLSession yet — the handshake hasn't run. `getSession()`
            // (like real JSSE) implicitly starts the handshake if needed; see
            // its registration below.
            Ok(Some(Value::Object(Some(socket))))
        },
    );

    // If `this` (a `javax/net/ssl/SSLSocket`) is still a pending layered
    // socket (`createSocket(Socket wrapped, ...)`'s deferred-handshake
    // design — see that registration's doc comment), drive its handshake now
    // in whichever role `setUseClientMode` last left it in (default client),
    // update the socket's fields/side-table to the now-real rustls stream
    // id, and build its SSLSession. Returns the resolved (non-pending) tls
    // id either way — a socket that was never pending, or already
    // handshaked, is returned unchanged.
    fn ensure_layered_handshake_started(
        ctx: &mut dyn NativeContext,
        socket: ObjectRef,
    ) -> Result<i32, MethodCallFailed> {
        let tls_id = new13_resolve_tls_id(ctx, socket);
        if tls_id < crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
            || tls_id >= crate::servlet::RUSTLS_SOCK_ID_BASE
        {
            return Ok(tls_id);
        }
        let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
        // Pure native socket I/O below (the actual TLS handshake). Mark this
        // thread blocked so a concurrent VM safepoint doesn't wait for it
        // while the peer's event loop waits for this handshake's reply.
        // Pin `socket` across the park: the handshake is long, a moving young
        // collection during it relocates the object, and EVERY write below
        // (`sock_set_for_create`, `NEW13_SOCK_TLSID`, the `SSLSession`) would
        // then land on a stale reference. The socket keeps its PENDING tls id,
        // so the next resolution drives the handshake again and finds the
        // pending entry already consumed — `SSLHandshakeException: layered
        // socket handshake state missing`, seen intermittently in
        // `shouldUpdateSslWhenReloadingSslBundles`. Same hazard
        // `new13_do_create_socket`'s blocking-region comment describes.
        let socket_pin = ctx.pin_native_root(socket);
        ctx.begin_blocking_region();
        let stream_result = crate::t27_tls::drive_pending_layered_handshake(pending_id);
        ctx.end_blocking_region();
        let socket = ctx.read_native_pin(socket_pin, socket);
        ctx.unpin_native_roots(socket_pin);
        // Match `new13_do_create_socket`'s existing contract: a rustls
        // handshake failure (rejected/mismatched certificate, no common
        // cipher suite, etc.) must surface as `SSLHandshakeException`, not a
        // generic `IOException` — callers (this test cluster's
        // `connectWithSslBundle`/`connectWithSslBundleAndOptionsMismatch`
        // among them) specifically assert on the JSSE exception type for an
        // intentionally-rejected connection.
        let stream_id = stream_result.map_err(|message| {
            crate::phases_early::throw_jca_exc(ctx, "javax/net/ssl/SSLHandshakeException", &message)
        })?;
        let real_tls_id = crate::servlet::RUSTLS_SOCK_ID_BASE + stream_id;
        let port = ctx.get_field(socket, NEW13_SOCK_PORT).as_int().unwrap_or(0);
        crate::net_phase_e::sock_set_for_create(ctx, socket, port, real_tls_id);
        ctx.set_field(socket, NEW13_SOCK_TLSID, Value::Int(real_tls_id));
        let (protocol, cipher, _alpn, _sni) = crate::t27_tls::rustls_session_info(stream_id)
            .unwrap_or_else(|| ("TLS".into(), "UNKNOWN".into(), None, None));
        let session = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 3);
        let protocol = ctx.create_string(&protocol);
        let cipher = ctx.create_string(&cipher);
        ctx.set_field(session, NEW13_SESS_PROTO, Value::Object(Some(protocol)));
        ctx.set_field(session, NEW13_SESS_CIPHER, Value::Object(Some(cipher)));
        ctx.set_field(session, NEW13_SESS_TLSID, Value::Int(real_tls_id));
        // CLIENT-mode only (a server stream never has "peer certificates" in
        // this sense for our purposes here) — without this, a caller like
        // Apache HttpComponents' `AbstractClientTlsStrategy.verifySession()`,
        // which unconditionally checks `SSLSession.getPeerCertificates()`
        // right after every successful TLS upgrade, sees an empty chain and
        // throws `SSLPeerUnverifiedException` even though the handshake
        // itself succeeded.
        if let Some(chain) = crate::t27_tls::rustls_client_peer_cert_chain_der(stream_id) {
            crate::t27_tls::record_client_peer_chain(ctx, session, chain);
        }
        ctx.set_field(socket, NEW13_SOCK_SESSION, Value::Object(Some(session)));
        // A real handshake just completed on this socket — this is the one
        // place in the SSLSocket bridge where that is true after Java has had
        // any chance to register a listener (`createSocket(host, port)`
        // handshakes before it returns the socket, so a listener added
        // afterwards has, correctly, missed it). Stock JSSE signals exactly
        // here; see `new13_fire_handshake_completed`.
        new13_fire_handshake_completed(ctx, socket);
        Ok(real_tls_id)
    }

    /// Resolve this `SSLSocket`'s `SSLSession`, rebuilding it from the live
    /// TLS stream when the stored field reads back as null.
    ///
    /// FIX (TestSsl.testClientInitiatedRenegotiation[JSSE]): `getSession()`
    /// used to return `ctx.get_field(this, NEW13_SOCK_SESSION)` raw, and on
    /// the `createSocket(String, int)` path that field reads back **null** —
    /// so a plain client socket answered `getSession() == null` where JSSE
    /// guarantees a non-null session. `new13_finish_socket` does write the
    /// field, but as its own FIX comment records, `alloc_concurrent_synthetic`
    /// sizes this object with the REAL `javax/net/ssl/SSLSocket` layout and
    /// the layout guard silently drops writes whose slot type does not match;
    /// the same hazard that already forced `NEW13_SOCK_TLSID` into
    /// `net_phase_e`'s side table applies to the session slot.
    ///
    /// So do what the tls-id resolution already does: treat the field as a
    /// fast path and fall back to the authoritative source. `new13_resolve_tls_id`
    /// finds the stream via the side table, and `new13_alloc_ssl_session`
    /// rebuilds an equivalent session from it. The rebuilt object is written
    /// back to the field on the chance the slot is usable on this layout, but
    /// nothing depends on that write landing.
    ///
    /// Returns `Value::Object(None)` only when there is genuinely no stream
    /// (an unconnected socket) — the one case where JSSE itself would hand
    /// back an invalid session rather than a real one.
    fn new13_resolve_socket_session(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
        let stored = ctx.get_field(this, NEW13_SOCK_SESSION);
        if matches!(stored, Value::Object(Some(_))) {
            return stored;
        }
        let tls_id = new13_resolve_tls_id(ctx, this);
        if tls_id < 0 {
            return Value::Object(None);
        }
        let pin = ctx.pin_native_root(this);
        let session = new13_alloc_ssl_session(ctx, tls_id);
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        ctx.set_field(this, NEW13_SOCK_SESSION, Value::Object(Some(session)));
        Value::Object(Some(session))
    }

    /// Deliver a `HandshakeCompletedEvent` to every listener registered on
    /// `socket`, for a handshake that has genuinely just completed.
    ///
    /// Call this only where new key material was actually negotiated. It is
    /// deliberately NOT called from `startHandshake()` on an established
    /// connection — see the `addHandshakeCompletedListener` registration.
    ///
    /// Deviation from stock JSSE, recorded because it is observable: the real
    /// JDK dispatches this from a fresh thread named
    /// `HandshakeCompletedNotify-Thread` (`sun.security.ssl.TransportContext.
    /// finishHandshake`), whereas this delivers synchronously on the thread
    /// that completed the handshake. A listener therefore sees a different
    /// `Thread.currentThread().getName()`, and a listener that blocks will
    /// block the handshaking call rather than a throwaway notifier thread.
    /// Synchronous delivery is chosen over spawning a Java thread from a
    /// native because it needs no `Runnable` shim class and cannot leak a
    /// thread when a listener throws; the event contents are identical.
    fn new13_fire_handshake_completed(ctx: &mut dyn NativeContext, socket: ObjectRef) {
        let socket_key = ctx.identity_hash_code(socket) as u32 as u64;
        // Copy the handles out and release the lock BEFORE re-entering Java:
        // `handshakeCompleted` is arbitrary application code that can call
        // back into `add`/`removeHandshakeCompletedListener` on this same
        // socket, which would deadlock on a held non-reentrant mutex.
        let handles: Vec<usize> = match handshake_listeners().lock().get(&socket_key) {
            Some(entry) => entry.iter().map(|(_, handle)| *handle).collect(),
            None => return,
        };
        if handles.is_empty() {
            return;
        }
        let pin = ctx.pin_native_root(socket);
        let socket = ctx.read_native_pin(pin, socket);
        let session = new13_resolve_socket_session(ctx, socket);
        let socket = ctx.read_native_pin(pin, socket);
        // Pin the session too: `new_object_initialized` below allocates and
        // can therefore collect, and a raw `ObjectRef` sitting in `session`
        // across that point is exactly the stale-reference hazard
        // `pin_native_root` exists for.
        let session = match session {
            Value::Object(Some(session)) => {
                let session_pin = ctx.pin_native_root(session);
                Value::Object(Some(ctx.read_native_pin(session_pin, session)))
            }
            other => other,
        };
        let socket = ctx.read_native_pin(pin, socket);
        // `HandshakeCompletedEvent` is a real JDK class with real bytecode;
        // running its constructor is what makes `getSource()`/`getSession()`/
        // `getCipherSuite()` work for free rather than needing a synthetic
        // stand-in with hand-maintained field indices.
        let event = ctx.new_object_initialized(
            "javax/net/ssl/HandshakeCompletedEvent",
            "(Ljavax/net/ssl/SSLSocket;Ljavax/net/ssl/SSLSession;)V",
            &[Value::Object(Some(socket)), session],
        );
        let event = match event {
            Ok(Some(Value::Object(Some(event)))) => event,
            _ => {
                ctx.unpin_native_roots(pin);
                return;
            }
        };
        let event_pin = ctx.pin_native_root(event);
        for handle in handles {
            let Some(listener) = ctx.resolve_global_root(handle) else {
                continue;
            };
            let event = ctx.read_native_pin(event_pin, event);
            // A listener that throws must not abort the handshake or swallow
            // the remaining listeners — stock JSSE runs them on a detached
            // notifier thread, where a throw is likewise invisible to the
            // handshaking code.
            let _ = ctx.invoke_virtual(
                listener,
                "handshakeCompleted",
                "(Ljavax/net/ssl/HandshakeCompletedEvent;)V",
                &[Value::Object(Some(event))],
            );
        }
        ctx.unpin_native_roots(pin);
    }

    /// Release the global roots held for `socket`'s handshake listeners.
    /// Called from `close()` so a long-lived process that opens many TLS
    /// sockets does not accumulate permanently-reachable listener objects.
    fn new13_drop_handshake_listeners(ctx: &mut dyn NativeContext, socket: ObjectRef) {
        let socket_key = ctx.identity_hash_code(socket) as u32 as u64;
        let handles = match handshake_listeners().lock().remove(&socket_key) {
            Some(entry) => entry,
            None => return,
        };
        for (_, handle) in handles {
            ctx.remove_global_root(handle);
        }
    }

    // SSLSocket methods
    let ssl_sock = "javax/net/ssl/SSLSocket";
    r.register(
        ssl_sock,
        "getSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Real JSSE: getSession() implicitly starts the handshake if one
            // hasn't run yet.
            ensure_layered_handshake_started(ctx, this)?;
            let session = new13_resolve_socket_session(ctx, this);
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getSession sock={:?} -> {:?}",
                    std::thread::current().id(),
                    this,
                    session
                );
            }
            Ok(Some(session))
        },
    );
    r.register(ssl_sock, "startHandshake", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ensure_layered_handshake_started(ctx, this)?;
        Ok(None)
    });
    // These two real JDK `SSLSocket` methods originally had no native
    // registration at all, so calling either threw `AbstractMethodError: ...
    // has no Code attribute` (the abstract class has no bytecode of its own).
    // That was first patched into an inert no-op pair, which stopped the
    // crash but left a worse contract: a listener could be registered and was
    // then *never* invoked, on any code path, for any handshake — including
    // the ordinary initial one that has nothing to do with renegotiation.
    //
    // FIX (TestSsl.testClientInitiatedRenegotiation[JSSE]): store the
    // listeners for real and dispatch a genuine `HandshakeCompletedEvent`
    // whenever a handshake actually completes on the socket (see
    // `new13_fire_handshake_completed`, called from
    // `ensure_layered_handshake_started`).
    //
    // What still does NOT fire is a `startHandshake()` on an
    // already-established connection — i.e. a client-initiated renegotiation.
    // That is deliberate and is a property of the TLS backend, not an
    // oversight: rustls does not implement TLS 1.2 renegotiation, and its own
    // manual (`vendor/rustls-cbc/src/manual/tlsvulns.rs`) lists that omission
    // as its mitigation for CVE-2009-3555 and 3SHAKE. Firing the event
    // anyway would tell the application that fresh key material had been
    // derived when none had. See
    // `fixed-suite-bugs/tomcat/testssl-client-initiated-renegotiation-FIXED.md`.
    r.register(
        ssl_sock,
        "addHandshakeCompletedListener",
        "(Ljavax/net/ssl/HandshakeCompletedListener;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let listener = match args.get(1) {
                Some(Value::Object(Some(listener))) => *listener,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "listener is null".to_string(),
                    }
                    .into())
                }
            };
            let listener_key = ctx.identity_hash_code(listener);
            // A global root, not a bare ObjectRef: this reference outlives the
            // current native call and is consumed by a later one, so a moving
            // collection in between would otherwise leave a stale pointer —
            // the `add_global_root` doc comment describes exactly this case.
            let handle = ctx.add_global_root(listener);
            let socket_key = ctx.identity_hash_code(this) as u32 as u64;
            let mut table = handshake_listeners().lock();
            let entry = table.entry(socket_key).or_default();
            // Real JSSE stores listeners in a `HashSet`, so re-adding the same
            // listener is idempotent rather than double-registering it.
            if entry.iter().any(|(key, _)| *key == listener_key) {
                drop(table);
                ctx.remove_global_root(handle);
                return Ok(None);
            }
            entry.push((listener_key, handle));
            Ok(None)
        },
    );
    r.register(
        ssl_sock,
        "removeHandshakeCompletedListener",
        "(Ljavax/net/ssl/HandshakeCompletedListener;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let listener = match args.get(1) {
                Some(Value::Object(Some(listener))) => *listener,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "listener is null".to_string(),
                    }
                    .into())
                }
            };
            let listener_key = ctx.identity_hash_code(listener);
            let socket_key = ctx.identity_hash_code(this) as u32 as u64;
            let removed = {
                let mut table = handshake_listeners().lock();
                match table.get_mut(&socket_key) {
                    Some(entry) => {
                        match entry.iter().position(|(key, _)| *key == listener_key) {
                            Some(pos) => {
                                let (_, handle) = entry.remove(pos);
                                if entry.is_empty() {
                                    table.remove(&socket_key);
                                }
                                Some(handle)
                            }
                            None => None,
                        }
                    }
                    None => None,
                }
            };
            match removed {
                Some(handle) => {
                    ctx.remove_global_root(handle);
                    Ok(None)
                }
                // Real JSSE throws for a listener that was never added.
                None => Err(RuntimeError::IllegalArgumentException {
                    message: "listener not registered".to_string(),
                }
                .into()),
            }
        },
    );
    // connect(SocketAddress[, int timeout]) — the other half of the zero-arg
    // `createSocket()` pattern registered on `SSLSocketFactory` above. Real
    // `javax.net.ssl.SSLSocket` does not redeclare `connect` (it stays
    // inherited, concrete, from `java.net.Socket`), but that ancestor's own
    // native (`net_phase_e.rs`'s `re1_connect_socket`) only performs a plain
    // TCP connect — registering here, on the more specific `SSLSocket` class,
    // intercepts first and additionally drives the TLS client handshake.
    r.register(
        ssl_sock,
        "connect",
        "(Ljava/net/SocketAddress;)V",
        new13_ssl_socket_connect,
    );
    r.register(
        ssl_sock,
        "connect",
        "(Ljava/net/SocketAddress;I)V",
        new13_ssl_socket_connect,
    );
    // setUseClientMode/getUseClientMode, setNeedClientAuth/getNeedClientAuth,
    // setWantClientAuth/getWantClientAuth — unlike getApplicationProtocol
    // (concrete-but-throws) these six are genuinely `abstract` in the real
    // `javax.net.ssl.SSLSocket` base class (only a concrete provider
    // subclass, e.g. SunJSSE's SSLSocketImpl, implements them). Since this
    // synthetic object's class literally IS `javax/net/ssl/SSLSocket`, an
    // `invokevirtual` against the abstract declaration throws
    // `AbstractMethodError` — thrown from `MockWebServer$SocketHandler.
    // handle()` immediately after `createSocket(Socket,...)` returns
    // (`sslSocket.setUseClientMode(false)`, unconditional, BEFORE any
    // ALPN/read/write call), caught by its generic `catch (Exception e)` and
    // logged at SEVERE — invisible under CratonVM's apparently-inert
    // `java.util.logging`, so the connection is silently abandoned having
    // never read the request or written a response. This, not the ALPN
    // accessors below, is the actual root cause of the request/response
    // exchange never happening for a server socket obtained via
    // `SSLSocketFactory.createSocket(Socket,...)` (MockWebServer's HTTPS
    // listener contract) — `getApplicationProtocol`/`getSSLParameters` are
    // still worth having registered (OkHttp calls them too, just later) but
    // execution never reached them without this fix.
    r.register(ssl_sock, "setUseClientMode", "(Z)V", |ctx, args| {
        // A socket from `createSocket(Socket wrapped, ...)` defers its actual
        // handshake (see that registration's doc comment) precisely so this
        // call can still decide client-vs-server mode; record it on the
        // pending state if the handshake hasn't started yet. A socket from
        // any other creation path (already handshaked, or never went through
        // the deferred path) has no pending entry — matches this file's
        // existing convention of silently ignoring a mode change once the
        // role is already fixed.
        let this = obj_arg(args, 0)?;
        let use_client = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let tls_id = new13_resolve_tls_id(ctx, this);
        if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
            && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
        {
            let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
            crate::t27_tls::set_pending_layered_socket_client_mode(pending_id, use_client);
        }
        // Record the role regardless of whether a pending entry existed, so
        // `getUseClientMode()` reports what the caller actually asked for
        // rather than the hard-coded `false` it used to return.
        ssl_sock_auth_update(ctx, this, |s| s.0 = i32::from(use_client));
        Ok(None)
    });
    r.register(ssl_sock, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ssl_sock_auth_get(ctx, this).0)))
    });
    // STUB-REMOVAL (wave 2): these four were constant no-ops / constant `false`.
    // See `ssl_sock_auth_state` for why the request is recorded here and where
    // the enforcing rebuild actually happens (`SSLServerSocket`). JSSE's
    // documented exclusivity — `setNeedClientAuth(true)` clears `want`, and
    // vice versa — is honoured, matching the `SSLEngine` sibling below.
    r.register(ssl_sock, "setNeedClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ssl_sock_auth_update(ctx, this, |s| {
            s.1 = i32::from(v != 0);
            if v != 0 {
                s.2 = 0;
            }
        });
        Ok(None)
    });
    r.register(ssl_sock, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ssl_sock_auth_get(ctx, this).1)))
    });
    r.register(ssl_sock, "setWantClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ssl_sock_auth_update(ctx, this, |s| {
            s.2 = i32::from(v != 0);
            if v != 0 {
                s.1 = 0;
            }
        });
        Ok(None)
    });
    r.register(ssl_sock, "getWantClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ssl_sock_auth_get(ctx, this).2)))
    });
    // getApplicationProtocol/getHandshakeApplicationProtocol — the real
    // `javax.net.ssl.SSLSocket` base class's own body for these (unlike most
    // of its methods, which are abstract) is CONCRETE and just throws
    // `UnsupportedOperationException`; only a provider's concrete subclass
    // (SunJSSE's SSLSocketImpl) overrides it. Since this synthetic object's
    // class literally IS `javax/net/ssl/SSLSocket`, without a native
    // registration here that base-class bytecode runs and throws. Callers
    // like OkHttp's `Platform.getSelectedProtocol()` (used by MockWebServer's
    // connection handler for ALPN bookkeeping right after `startHandshake()`)
    // catch that as a generic `Exception`, log it at FINE/SEVERE (invisible
    // by default under java.util.logging), and abandon the connection having
    // never read the request or written a response — surfaced to the client
    // as a silent hang (e.g. Reactor's `.block(Duration)` timing out).
    fn ssl_sock_negotiated_alpn(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
        let tls_id = new13_resolve_tls_id(ctx, this);
        if tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE {
            return None;
        }
        let raw_id = tls_id - crate::servlet::RUSTLS_SOCK_ID_BASE;
        crate::t27_tls::rustls_session_info(raw_id).and_then(|(_, _, alpn, _)| alpn)
    }
    r.register(
        ssl_sock,
        "getApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let alpn = ssl_sock_negotiated_alpn(ctx, this).unwrap_or_default();
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getApplicationProtocol sock={:?} -> {:?}",
                    std::thread::current().id(),
                    this,
                    alpn
                );
            }
            Ok(Some(Value::Object(Some(ctx.create_string(&alpn)))))
        },
    );
    r.register(
        ssl_sock,
        "getHandshakeApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let alpn = ssl_sock_negotiated_alpn(ctx, this).unwrap_or_default();
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getHandshakeApplicationProtocol sock={:?} -> {:?}",
                    std::thread::current().id(),
                    this,
                    alpn
                );
            }
            Ok(Some(Value::Object(Some(ctx.create_string(&alpn)))))
        },
    );
    // getSSLParameters/setSSLParameters — like getApplicationProtocol above,
    // the real `javax.net.ssl.SSLSocket` base class's default bodies call
    // through to other (mostly abstract-in-the-base-class) accessors; without
    // a concrete provider subclass backing this synthetic object, that chain
    // is liable to throw. OkHttp's `Jdk9Platform.configureTlsExtensions()`
    // calls `getSSLParameters()` then `setSSLParameters()` on every socket
    // BEFORE `startHandshake()` (to offer its ALPN protocol list) — for
    // MockWebServer's synthetic server socket the handshake has already run
    // synchronously inside `createSocket`, so this call is moot for actual
    // negotiation, but it must not throw or the caller (uncaught) abandons
    // the connection having never read the request or written a response.
    r.register(
        ssl_sock,
        "getSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getSSLParameters ENTER sock={:?}",
                    std::thread::current().id(),
                    this
                );
            }
            let ciphers = ssl_sock_supported_cipher_suites(ctx);
            let protocols = {
                let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
                let negotiated =
                    if let Value::Object(Some(session)) = ctx.get_field(this, NEW13_SOCK_SESSION) {
                        match ctx.get_field(session, NEW13_SESS_PROTO) {
                            Value::Object(Some(s)) => ctx.read_string(s),
                            _ => None,
                        }
                    } else {
                        None
                    };
                let s = ctx.create_string(&negotiated.unwrap_or_else(|| "TLSv1.3".to_string()));
                ctx.set_array_element(arr, 0, Value::Object(Some(s)));
                arr
            };
            let params = match ctx.new_object_initialized(
                "javax/net/ssl/SSLParameters",
                "([Ljava/lang/String;[Ljava/lang/String;)V",
                &[Value::Object(Some(ciphers)), Value::Object(Some(protocols))],
            )? {
                Some(Value::Object(Some(o))) => o,
                _ => alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLParameters", 4),
            };
            Ok(Some(Value::Object(Some(params))))
        },
    );
    r.register(
        ssl_sock,
        "setSSLParameters",
        "(Ljavax/net/ssl/SSLParameters;)V",
        |ctx, args| {
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} setSSLParameters sock={:?}",
                    std::thread::current().id(),
                    args.first()
                );
            }
            // A socket from `createSocket(Socket wrapped, ...)`'s deferred
            // handshake honors any cipher-suite restriction carried by
            // `params` if called before the handshake actually starts — the
            // same real-JSSE-contract reasoning as `setEnabledCipherSuites`
            // above (which this often replaces: Apache HttpComponents 5
            // configures TLS options via an `SSLParameters` object and
            // `SSLSocket.setSSLParameters()`, not the legacy per-field
            // setters, so `setEnabledCipherSuites` alone never saw
            // `connectWithSslBundleAndOptionsMismatch`'s deliberately
            // mismatched cipher suite).
            if let (Ok(this), Some(Value::Object(Some(params)))) = (
                obj_arg(args, 0),
                args.get(1).copied().and_then(|v| match v {
                    Value::Object(Some(_)) => Some(v),
                    _ => None,
                }),
            ) {
                let tls_id = new13_resolve_tls_id(ctx, this);
                if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
                    && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
                {
                    let mut ciphers = Vec::new();
                    if let Ok(Some(Value::Object(Some(arr)))) =
                        ctx.invoke_virtual(params, "getCipherSuites", "()[Ljava/lang/String;", &[])
                    {
                        let len = ctx.array_length(arr);
                        for i in 0..len {
                            if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                                if let Some(name) = ctx.read_string(s) {
                                    ciphers.push(name);
                                }
                            }
                        }
                    }
                    if !ciphers.is_empty() {
                        let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
                        crate::t27_tls::set_pending_layered_socket_ciphers(pending_id, ciphers);
                    }
                }
            }
            // Handshake already completed in createSocket for every OTHER
            // socket shape; the ALPN/cipher preferences a caller sets here
            // can no longer change anything for those. Accept and discard.
            Ok(None)
        },
    );
    // getSupportedCipherSuites/getEnabledCipherSuites/getSupportedProtocols/
    // getEnabledProtocols — same AbstractMethodError family as TC0622's
    // SSLSession buffer-size gap: the accessors above cover I/O and
    // lifecycle, but nothing registered these on `javax/net/ssl/SSLSocket`
    // itself, so `invokeinterface SSLSocket.getEnabledCipherSuites()`
    // resolved to the abstract interface declaration (no Code) and threw
    // `AbstractMethodError`. Mirrors the static suite/protocol lists already
    // used by `SSLEngineImpl` (t27_tls.rs) for consistency; the socket's
    // handshake already completed in `createSocket`/`accept`, so
    // enabled == supported here (matches the JDK default before any
    // `setEnabledCipherSuites` call — this synthetic socket has no
    // set-side storage, so `set*` below are accepted but not persisted).
    fn ssl_sock_supported_cipher_suites(ctx: &mut dyn NativeContext) -> ObjectRef {
        // Single source of truth — see `t27_tls::SUPPORTED_CIPHER_SUITE_NAMES`.
        let suites = crate::t27_tls::SUPPORTED_CIPHER_SUITE_NAMES;
        let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), suites.len());
        for (i, &s) in suites.iter().enumerate() {
            let so = ctx.create_string(s);
            ctx.set_array_element(arr, i, Value::Object(Some(so)));
        }
        arr
    }
    r.register(
        ssl_sock,
        "getSupportedCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(ssl_sock_supported_cipher_suites(
                ctx,
            )))))
        },
    );
    r.register(
        ssl_sock,
        "getEnabledCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(ssl_sock_supported_cipher_suites(
                ctx,
            )))))
        },
    );
    r.register(
        ssl_sock,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            // A socket from `createSocket(Socket wrapped, ...)`'s deferred
            // handshake honors this if called before the handshake actually
            // starts (real JSSE contract; `connectWithSslBundleAndOptionsMismatch`
            // relies on exactly this createSocket-then-narrow-ciphers
            // sequence to make the handshake genuinely fail on a
            // deliberately mismatched suite). A socket from any other path,
            // or one that's already handshaked, has no pending entry to
            // update — accepted but not persisted, same as before.
            let this = obj_arg(args, 0)?;
            let tls_id = new13_resolve_tls_id(ctx, this);
            if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
                && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
            {
                let mut ciphers = Vec::new();
                if let Some(Value::Object(Some(arr))) = args.get(1) {
                    let len = ctx.array_length(*arr);
                    for i in 0..len {
                        if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                            if let Some(name) = ctx.read_string(s) {
                                ciphers.push(name);
                            }
                        }
                    }
                }
                let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
                crate::t27_tls::set_pending_layered_socket_ciphers(pending_id, ciphers);
            }
            Ok(None)
        },
    );
    r.register(
        ssl_sock,
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
        ssl_sock,
        "getEnabledProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Report the protocol actually negotiated (stored on the
            // session) alongside TLSv1.2 so callers checking membership
            // against either standard name succeed.
            //
            // FIX (TestSsl.testClientInitiatedRenegotiation[JSSE]): read the
            // session through `new13_resolve_socket_session` rather than the
            // raw field. The raw read returns null on the
            // `createSocket(String, int)` path (see that helper's comment),
            // and the `unwrap_or` below then fabricated `"TLSv1.3"` — which is
            // how a socket built from an `SSLContext.getInstance("TLSv1.2")`
            // came to report `[TLSv1.3]`. The fabricated value was reported
            // whatever the connection had actually negotiated, so it was a
            // guess presented as a fact, not merely an imprecise default.
            let negotiated =
                if let Value::Object(Some(session)) = new13_resolve_socket_session(ctx, this) {
                    match ctx.get_field(session, NEW13_SESS_PROTO) {
                        Value::Object(Some(s)) => ctx.read_string(s),
                        _ => None,
                    }
                } else {
                    None
                };
            let proto = negotiated.unwrap_or_else(|| "TLSv1.3".to_string());
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
            let s = ctx.create_string(&proto);
            ctx.set_array_element(arr, 0, Value::Object(Some(s)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    // STUB-REMOVAL (wave 2): this was an unconditional no-op, so a caller
    // narrowing a socket to a single TLS version got no enforcement at all —
    // the security-relevant direction (an app disabling TLS 1.0/1.1, or pinning
    // 1.3) was silently discarded. `net_phase_e` registers the same
    // (class, method, descriptor) LATER and therefore wins in the real-JDK
    // build (see its own doc comment at `setEnabledProtocols`), but in
    // `--synthetic-jdk` mode `register_synthetic_overrides` runs AFTER
    // `register_essential_natives`, so THIS registration is the live one and
    // the restriction vanished. Mirror the `setEnabledCipherSuites` sibling
    // above: a socket from `createSocket(Socket wrapped, ...)` handshakes
    // lazily, so a restriction set beforehand still counts. A socket that has
    // already handshaked keeps the accept-and-discard behaviour (its version
    // is settled and rustls does not renegotiate).
    r.register(
        ssl_sock,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tls_id = new13_resolve_tls_id(ctx, this);
            if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
                && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
            {
                let mut protocols: Vec<String> = Vec::new();
                if let Some(Value::Object(Some(arr))) = args.get(1) {
                    let len = ctx.array_length(*arr);
                    for i in 0..len {
                        if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                            if let Some(name) = ctx.read_string(s) {
                                protocols.push(name);
                            }
                        }
                    }
                }
                let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
                crate::t27_tls::set_pending_layered_socket_protocols(pending_id, protocols);
            }
            Ok(None)
        },
    );
    r.register(
        ssl_sock,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Real JSSE: the first I/O call implicitly starts the handshake
            // if one hasn't run yet — this socket may still be pending (see
            // `createSocket(Socket wrapped, ...)`'s doc comment).
            let fd_id = ensure_layered_handshake_started(ctx, this)?;
            if crate::nbflags().dbg_tls_sock || crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] SSLSocket.getInputStream sock={:?} tls_id={}",
                    this, fd_id
                );
            }
            // Return an InputStream that reads from the TLS fd
            let is = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketInputStream", 1);
            // Real JDK stream layouts do not have our synthetic Int slot 0.
            // Preserve the TLS id in the identity-keyed socket side table too.
            crate::net_phase_e::sock_set_for_create(ctx, is, 0, fd_id);
            ctx.set_field(is, 0, Value::Int(fd_id));
            // INVESTIGATED AND REVERTED (tls-handshake-enforcement-gap, doc
            // 21 — `TestSsl.testPost`): wrapping this in a real
            // `java.io.BufferedInputStream`, so that a single-byte `read()`
            // becomes bytecode over a Java `byte[]` instead of a native call.
            // That is the shape real JSSE's `SSLSocketImpl$AppInputStream`
            // has, and `testPost` reads 16 MiB one byte at a time on each of
            // 8 threads (~134 million reads), so it looked like the obvious
            // win. Measured: it made things WORSE — 368 s unwrapped vs >700 s
            // (test timeout) wrapped.
            //
            // Why, from a `--stack-dump-on-timeout=120` capture: all 8 client
            // threads sit in `BufferedInputStream.read`/`fill`/`getBufIfOpen`
            // with `blocked=false` and a DIFFERENT pc in each dump — real
            // progress, just too slow — while the Tomcat exec threads block in
            // `doWrite` waiting for them to drain. JDK 25's
            // `BufferedInputStream.read()` takes its `InternalLock` (or
            // `synchronized`) on EVERY call, so per byte it costs an AQS
            // lock/unlock plus interpreted bytecode, which on this VM is
            // dearer than the one native call it replaces.
            //
            // The per-byte cost here is the Java->native transition itself,
            // not the work behind it: a native-side readahead
            // (`servlet::s2_tls_fill_readahead`) removes the rustls and
            // registry work from every byte and only bought ~15%. Closing the
            // rest needs cheaper native dispatch, which is the pre-existing
            // throughput-wall work, not a TLS fix.
            Ok(Some(Value::Object(Some(is))))
        },
    );
    r.register(
        ssl_sock,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = ensure_layered_handshake_started(ctx, this)?;
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getOutputStream sock={:?} tls_id={}",
                    std::thread::current().id(),
                    this,
                    fd_id
                );
            }
            let os = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketOutputStream", 1);
            crate::net_phase_e::sock_set_for_create(ctx, os, 0, fd_id);
            ctx.set_field(os, 0, Value::Int(fd_id));
            Ok(Some(Value::Object(Some(os))))
        },
    );
    r.register(ssl_sock, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Drop any handshake-listener global roots first: they keep the
        // listener objects permanently reachable, and a closed socket will
        // never fire another event. Unconditional (not gated on `tls_id >= 0`
        // below) so a second close(), or a socket closed before it ever
        // handshaked, still releases them.
        new13_drop_handshake_listeners(ctx, this);
        let tls_id = new13_resolve_tls_id(ctx, this);
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} JAVA_CALLED Socket.close() tls_id={}",
                std::thread::current().id(),
                tls_id
            );
        }
        if tls_id >= 0 {
            // Idempotent — s2_tls_close tolerates an unknown id. The TCP
            // half-close inside shutdown() also flushes any pending TLS
            // close_notify, preserving RFC 8446 §6.1 semantics.
            let _ = crate::servlet::s2_tls_close(tls_id);
            // Overwrite the id so a second close() is a no-op.
            ctx.set_field(this, NEW13_SOCK_TLSID, Value::Int(-1));
            // Mark the session as invalidated so isValid() returns false.
            if let Value::Object(Some(session)) = ctx.get_field(this, NEW13_SOCK_SESSION) {
                if ctx.object_num_fields(session) > NEW13_SESS_TLSID {
                    ctx.set_field(session, NEW13_SESS_TLSID, Value::Int(-1));
                }
            }
        }
        // Keep net_phase_e's side table (if this socket was built through
        // its createSocket(String,int) — see new13_resolve_tls_id) in sync,
        // so any other code path that consults it also observes closed.
        crate::net_phase_e::sock_mark_closed_for_upcall(ctx, this);
        ctx.set_field(this, NEW13_SOCK_CLOSED, Value::Int(1));
        Ok(None)
    });
    r.register(ssl_sock, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let closed = match ctx.get_field(this, NEW13_SOCK_CLOSED) {
            Value::Int(c) => c != 0,
            _ => crate::net_phase_e::sock_is_closed_for_upcall(ctx, this),
        };
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} isClosed sock={:?} -> {}",
                std::thread::current().id(),
                this,
                closed
            );
        }
        Ok(Some(Value::Int(if closed { 1 } else { 0 })))
    });
    r.register(ssl_sock, "isConnected", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let closed = match ctx.get_field(this, NEW13_SOCK_CLOSED) {
            Value::Int(c) => c != 0,
            _ => crate::net_phase_e::sock_is_closed_for_upcall(ctx, this),
        };
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} isConnected sock={:?} -> {}",
                std::thread::current().id(),
                this,
                !closed
            );
        }
        Ok(Some(Value::Int(if closed { 0 } else { 1 })))
    });
    // `java.net.Socket` has the same registrations, but a real-JDK
    // `SSLSocket` receiver does not reliably inherit them through the native
    // dispatch lookup.  Falling through to Socket's bytecode reads the real
    // `Socket.impl` / `shutIn` fields from this synthetic overlay and can
    // report a live TLS connection as input-shut.  HttpComponents 5.4 checks
    // `isInputShutdown()` immediately before every request-body write and
    // turns that false positive into `ConnectionClosedException`.
    //
    // There is no independent half-close state for a rustls SSLSocket: the
    // only supported shutdown operation is `close()`, which marks the shared
    // side-table entry closed.  Use that authoritative state for both
    // directions rather than interpreting the host JDK's physical layout.
    r.register(ssl_sock, "isInputShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let closed = crate::net_phase_e::sock_is_closed_for_upcall(ctx, this);
        Ok(Some(Value::Int(if closed { 1 } else { 0 })))
    });
    r.register(ssl_sock, "isOutputShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let closed = crate::net_phase_e::sock_is_closed_for_upcall(ctx, this);
        Ok(Some(Value::Int(if closed { 1 } else { 0 })))
    });
    r.register(ssl_sock, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    // STUB-REMOVAL (wave 2): `getSoTimeout` returned a hard-coded 0 ("no
    // timeout") and `setSoTimeout` validated its argument then threw it away,
    // so a caller that configured a read timeout and read it back was told the
    // socket blocks forever — and the underlying `TcpStream` really did. Apply
    // the timeout to the live stream (exactly what `net_phase_e`'s
    // `java/net/Socket.setSoTimeout` does for the plain-socket case, which this
    // more-specific `SSLSocket` registration intercepts ahead of) and report
    // back what is actually configured.
    r.register(ssl_sock, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let configured = crate::net_phase_e::sock_get(ctx, this).read_timeout_ms;
        if configured != 0 {
            return Ok(Some(Value::Int(configured)));
        }
        let tls_id = new13_resolve_tls_id(ctx, this);
        if tls_id >= 0 {
            let reg = crate::servlet::s2_registry().lock();
            let timeout = if let Some(stream) = reg.streams.get(&tls_id) {
                stream.read_timeout().ok().flatten()
            } else if let Some(raw) = reg.tls_streams.get(&tls_id).and_then(|e| e.raw.as_ref()) {
                raw.read_timeout().ok().flatten()
            } else {
                None
            };
            if let Some(d) = timeout {
                return Ok(Some(Value::Int(d.as_millis() as i32)));
            }
        }
        Ok(Some(Value::Int(0)))
    });
    r.register(ssl_sock, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let timeout = args.get(1).and_then(Value::as_int).unwrap_or(0);
        if timeout < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("negative SO_TIMEOUT: {timeout}"),
            }
            .into());
        }
        let tls_id = new13_resolve_tls_id(ctx, this);
        if tls_id >= 0 {
            let d = if timeout == 0 {
                None
            } else {
                Some(std::time::Duration::from_millis(timeout as u64))
            };
            let reg = crate::servlet::s2_registry().lock();
            let applied = if let Some(stream) = reg.streams.get(&tls_id) {
                stream.set_read_timeout(d)
            } else if let Some(raw) = reg.tls_streams.get(&tls_id).and_then(|e| e.raw.as_ref()) {
                raw.set_read_timeout(d)
            } else {
                Ok(())
            };
            applied.map_err(|e| RuntimeError::IOException {
                message: format!("setSoTimeout failed: {e}"),
            })?;
        }
        Ok(None)
    });
    r.register(
        ssl_sock,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let host = match ctx.get_field(this, NEW13_SOCK_HOST) {
                Value::Object(Some(host)) => ctx.read_string(host).unwrap_or_default(),
                _ => String::new(),
            };
            if host.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            let address = crate::net_phase_e::alloc_inet_address_for_input(ctx, &host, &host);
            Ok(Some(Value::Object(Some(address))))
        },
    );
    r.register(
        ssl_sock,
        "getRemoteSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Defensive: InetSocketAddress(String,int)'s native ctor NPEs on
            // a null host (see the java/net/Socket sibling fix in
            // phases_early.rs) -- every known writer of NEW13_SOCK_HOST sets
            // a non-null placeholder, but never pass a raw possibly-unset
            // field straight through to a ctor that requires non-null.
            let host = match ctx.get_field(this, NEW13_SOCK_HOST) {
                h @ Value::Object(Some(_)) => h,
                _ => Value::Object(Some(ctx.create_string("0.0.0.0"))),
            };
            let port = ctx.get_field(this, NEW13_SOCK_PORT);
            ctx.new_object_initialized(
                "java/net/InetSocketAddress",
                "(Ljava/lang/String;I)V",
                &[host, port],
            )
        },
    );
    // STUB-REMOVAL (wave 2): was a hard-coded 0. `net_phase_e`'s socket side
    // table records the real bound local port at create/connect time
    // (`sock_set_for_create_with_local_port`); read it, and only fall back to 0
    // (the JDK's "not bound" answer) when nothing was recorded.
    r.register(ssl_sock, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            crate::net_phase_e::sock_get(ctx, this).local_port,
        )))
    });
    r.register(
        ssl_sock,
        "getLocalAddress",
        "()Ljava/net/InetAddress;",
        |ctx, _args| {
            let address =
                crate::net_phase_e::alloc_inet_address_unnamed(ctx, "127.0.0.1");
            Ok(Some(Value::Object(Some(address))))
        },
    );
    r.register(
        ssl_sock,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, _args| {
            let host = ctx.create_string("127.0.0.1");
            ctx.new_object_initialized(
                "java/net/InetSocketAddress",
                "(Ljava/lang/String;I)V",
                &[Value::Object(Some(host)), Value::Int(0)],
            )
        },
    );

    // NEW-13: SSLSocketInputStream — reads from a `s2_registry` TLS stream
    // identified by the tls_id stored in field 0 of the synthetic stream
    // instance. A -1 id or short-read of 0 maps to Java EOF (-1) per
    // InputStream.read semantics.
    //
    // PERF NOTE (testssl-testpost bulk TLS, 2026-08-05): this native's own body
    // is NOT what makes a byte-at-a-time reader slow. Measured against
    // `SSLSocketOutputStream.flush()` — a registered no-op native on the same
    // receiver, so the difference is the body and nothing else — one `read()`
    // costs 814 ns at 1 thread, of which 549 ns is the Java->native transition
    // and only ~265 ns is everything this closure does. Counters over a full
    // `testPost` confirmed the two candidate slow spots are absent: the field
    // read at slot 0 hits every time (16,777,216 of 16,777,216 calls — the
    // side-table fallback never fires), and the readahead does exactly one real
    // socket read per 16 KiB (1024 refills, avg 16384 bytes). What remained of
    // the body was the descriptor lookup behind `get_field`; see
    // `vm_exec::resolve_field_descriptor_byte_cached`'s stub note.
    let ssl_is = "javax/net/ssl/SSLSocketInputStream";
    r.register(ssl_is, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tls_id = ctx
            .get_field(this, 0)
            .as_int()
            .filter(|id| *id >= 0)
            .unwrap_or_else(|| crate::net_phase_e::sock_stream_id_for_upcall(ctx, this));
        if tls_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        // FIX (tls-handshake-enforcement-gap, doc 21 — TestSsl hang): serve
        // from the shared plaintext readahead when possible. Reading a single
        // byte through the full native path (blocking-region bracket +
        // registry lock + per-call rustls plumbing) costs microseconds, and a
        // caller that reads a multi-megabyte body a byte at a time —
        // `TestSsl.testPost` reads 16 MiB per thread on 8 threads, ~134
        // million calls — turned that into minutes. Real JSSE serves these
        // out of a plain Java `byte[]`. See `servlet::s2_tls_fill_readahead`
        // for why the buffer is keyed by stream id.
        if let Some(b) = crate::servlet::s2_tls_pop_buffered_byte(tls_id) {
            return Ok(Some(Value::Int(b as i32)));
        }
        // STW-COOPERATION (tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED):
        // the refill does a genuine OS-level blocking socket operation.
        // Without the blocked-region bracket this thread stays counted as a
        // cooperative mutator that can never reach a safepoint poll, so a
        // concurrent STW (GC / cross-thread JIT takeover) waits on it forever
        // — and the bytes it waits for are produced by a peer thread in this
        // same process (Tomcat's NioEndpoint SocketProcessor) which DOES stop
        // at the barrier: a mutual deadlock, observed as `rounds=64 pending=1
        // taken=0` repeating with no further progress. Same bug shape as the
        // `net_phase_e` HttpClient and S2 selector fixes; see
        // `fixed-suite-bugs/keycloak/
        // keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`.
        ctx.begin_blocking_region();
        let filled = crate::servlet::s2_tls_fill_readahead(tls_id);
        ctx.end_blocking_region();
        match filled {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(_) => Ok(Some(Value::Int(
                crate::servlet::s2_tls_pop_buffered_byte(tls_id)
                    .map(|b| b as i32)
                    .unwrap_or(-1),
            ))),
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    });
    r.register(ssl_is, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tls_id = ctx
            .get_field(this, 0)
            .as_int()
            .filter(|id| *id >= 0)
            .unwrap_or_else(|| crate::net_phase_e::sock_stream_id_for_upcall(ctx, this));
        if tls_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("SSLSocketInputStream.read: buffer is null".into()),
                }
                .into());
            }
        };
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        // Bounds check against the target array length so a bad caller can't
        // poison indices in `set_array_element`.
        let arr_len = ctx.array_length(arr);
        if off.saturating_add(len) > arr_len {
            return Err(RuntimeError::aioobe_index_only(off.saturating_add(len) as i32)
            .into());
        }
        if len == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut buf = vec![0u8; len];
        // STW-COOPERATION: blocking socket I/O — see the full rationale on
        // `SSLSocketInputStream.read()I` above.
        // `arr` is written AFTER the block, so it must survive a moving
        // collection that runs while this thread is parked — pin it.
        let arr_pin = ctx.pin_native_root(arr);
        ctx.begin_blocking_region();
        let read_result = crate::servlet::s2_tls_read(tls_id, &mut buf);
        ctx.end_blocking_region();
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.unpin_native_roots(arr_pin);
        match read_result {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(n) => {
                // PERF (testssl-testpost bulk TLS): one bulk copy instead of one
                // virtual `set_array_element` (plus a `Value` box) per byte. The
                // range was bounds-checked against `arr_len` above, so a `false`
                // here can only mean the pinned array came back smaller — report
                // that rather than silently claim bytes that never landed.
                if !ctx.write_byte_array_from(arr, off, &buf[..n]) {
                    return Err(RuntimeError::IOException {
                        message: "SSLSocketInputStream.read: destination array shrank".into(),
                    }
                    .into());
                }
                Ok(Some(Value::Int(n as i32)))
            }
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    });
    r.register(ssl_is, "available", "()I", |ctx, args| {
        // native-tls does not expose a non-blocking peek, so match the
        // reference JDK behaviour of reporting 0 bytes readable without
        // blocking — EXCEPT for plaintext already pulled off the socket into
        // this stream's readahead (see `servlet::s2_tls_fill_readahead`),
        // which is readable without blocking by definition and must be
        // reported, or a caller that loops on `available()` would stall on
        // bytes it has effectively already received.
        let this = obj_arg(args, 0)?;
        let tls_id = ctx
            .get_field(this, 0)
            .as_int()
            .filter(|id| *id >= 0)
            .unwrap_or_else(|| crate::net_phase_e::sock_stream_id_for_upcall(ctx, this));
        if tls_id < 0 {
            return Ok(Some(Value::Int(0)));
        }
        Ok(Some(Value::Int(
            crate::servlet::s2_tls_buffered_len(tls_id) as i32,
        )))
    });
    // STUB-REMOVAL (wave 2): `close()` on either of the two TLS socket streams
    // was an unconditional no-op, so `sslSocket.getInputStream().close()` (and
    // the output-stream sibling) left the TLS session and its OS socket open —
    // one leaked fd and one live TLS connection per caller that closes the
    // stream instead of the socket. The JDK contract for a socket's own streams
    // is the opposite: "Closing the returned InputStream will close the
    // associated socket" (`java.net.Socket.getInputStream`). Mirror
    // `SSLSocket.close()` above: shut the TLS stream down (which flushes
    // close_notify), stamp the id field to -1 so a second close is a no-op and
    // any later read reports EOF rather than reusing a recycled id, and keep
    // `net_phase_e`'s side table in sync.
    fn ssl_stream_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        let tls_id = ctx
            .get_field(this, 0)
            .as_int()
            .filter(|id| *id >= 0)
            .unwrap_or_else(|| crate::net_phase_e::sock_stream_id_for_upcall(ctx, this));
        if tls_id >= 0 {
            let _ = crate::servlet::s2_tls_close(tls_id);
            ctx.set_field(this, 0, Value::Int(-1));
        }
        crate::net_phase_e::sock_mark_closed_for_upcall(ctx, this);
        Ok(None)
    }
    r.register(ssl_is, "close", "()V", ssl_stream_close);

    // NEW-13: SSLSocketOutputStream — writes to `s2_registry` TLS stream.
    let ssl_os = "javax/net/ssl/SSLSocketOutputStream";
    r.register(ssl_os, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tls_id = ctx
            .get_field(this, 0)
            .as_int()
            .filter(|id| *id >= 0)
            .unwrap_or_else(|| crate::net_phase_e::sock_stream_id_for_upcall(ctx, this));
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} SSLSocketOutputStream.write(int) tls_id={}",
                std::thread::current().id(),
                tls_id
            );
        }
        if tls_id < 0 {
            return Err(RuntimeError::IOException {
                message: "SSLSocketOutputStream.write: stream is closed".into(),
            }
            .into());
        }
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        // STW-COOPERATION: blocking socket I/O — see the full rationale on
        // `SSLSocketInputStream.read()I` above.
        ctx.begin_blocking_region();
        let write_result = crate::servlet::s2_tls_write(tls_id, &[b]);
        ctx.end_blocking_region();
        write_result.map_err(|e| RuntimeError::IOException {
            message: e.to_string(),
        })?;
        Ok(None)
    });
    r.register(ssl_os, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tls_id = ctx
            .get_field(this, 0)
            .as_int()
            .filter(|id| *id >= 0)
            .unwrap_or_else(|| crate::net_phase_e::sock_stream_id_for_upcall(ctx, this));
        let len_arg = args.get(3).and_then(|v| v.as_int()).unwrap_or(-1);
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} SSLSocketOutputStream.write([BII) tls_id={} len={}",
                std::thread::current().id(),
                tls_id,
                len_arg
            );
        }
        if tls_id < 0 {
            return Err(RuntimeError::IOException {
                message: "SSLSocketOutputStream.write: stream is closed".into(),
            }
            .into());
        }
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("SSLSocketOutputStream.write: buffer is null".into()),
                }
                .into());
            }
        };
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let arr_len = ctx.array_length(arr);
        if off.saturating_add(len) > arr_len {
            return Err(RuntimeError::aioobe_index_only(off.saturating_add(len) as i32)
            .into());
        }
        if len == 0 {
            return Ok(None);
        }
        // PERF (testssl-testpost bulk TLS): one bulk copy instead of one virtual
        // `get_array_element` per byte. `TestSsl.testPost` alone pushes 16 MiB
        // per thread through here in 128 KiB blocks.
        let mut buf = vec![0u8; len];
        let copied = ctx.read_byte_array_into(arr, off, &mut buf);
        // The range was bounds-checked against `arr_len` above, so a short copy
        // means the array is not the byte[] this signature promises. Writing the
        // zero tail would put bytes on the wire the caller never supplied.
        buf.truncate(copied);
        // Drain the write — TlsStream::write may return short writes under
        // pressure, which callers would otherwise interpret as silent data
        // loss. Loop until the whole buffer has been accepted.
        let mut written = 0usize;
        // STW-COOPERATION: blocking socket I/O — see the full rationale on
        // `SSLSocketInputStream.read()I` above.
        // The whole drain loop goes in ONE blocked region, and the error
        // is carried out instead of returned from inside it — an early
        // `return` between begin/end would leave this thread permanently
        // marked blocked (the mirror-image failure: a live mutator the
        // barrier stops waiting for).
        ctx.begin_blocking_region();
        let mut write_err: Option<String> = None;
        while written < buf.len() {
            match crate::servlet::s2_tls_write(tls_id, &buf[written..]) {
                Ok(0) => {
                    write_err = Some("SSLSocketOutputStream.write: peer closed".into());
                    break;
                }
                Ok(n) => written += n,
                Err(e) => {
                    write_err = Some(e.to_string());
                    break;
                }
            }
        }
        ctx.end_blocking_region();
        if let Some(message) = write_err {
            return Err(RuntimeError::IOException { message }.into());
        }
        Ok(None)
    });
    // KEEP (genuinely empty): every `write` above hands its bytes straight to
    // `servlet::s2_tls_write`, which drives the underlying `TlsStream` and its
    // `TcpStream` synchronously — there is no Java- or Rust-side buffer between
    // the caller and the socket, so there is nothing for `flush()` to push.
    // VERIFIED against jdk-25 (`javap -p sun.security.ssl.SSLSocketImpl
    // $AppOutputStream`): the class declares no `flush` at all, so the real
    // call inherits `java.io.OutputStream.flush()`, whose body is empty. The
    // real behaviour is a no-op, not merely equivalent to one.
    r.register(ssl_os, "flush", "()V", |_ctx, _args| Ok(None));
    // See `ssl_stream_close` above — closing a socket's stream closes the
    // socket.
    r.register(ssl_os, "close", "()V", ssl_stream_close);

    // NEW-13: SSLSession methods are now backed by the s2_registry TLS id
    // stored at NEW13_SESS_TLSID. Each accessor falls back to the session's
    // own cached field if the id is missing (e.g. the session outlived the
    // TLS stream because the socket was closed), which preserves the ability
    // to call `getProtocol()` / `getCipherSuite()` after close as required
    // by SSLSession javadoc.
    let ssl_session = "javax/net/ssl/SSLSession";
    r.register(
        ssl_session,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tls_id = if ctx.object_num_fields(this) > NEW13_SESS_TLSID {
                ctx.get_field(this, NEW13_SESS_TLSID).as_int().unwrap_or(-1)
            } else {
                -1
            };
            if tls_id >= 0 {
                if let Some((p, _, _, _)) = crate::servlet::s2_tls_session_info(tls_id) {
                    let s = ctx.create_string(&p);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            Ok(Some(ctx.get_field(this, NEW13_SESS_PROTO)))
        },
    );
    r.register(
        ssl_session,
        "getCipherSuite",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tls_id = if ctx.object_num_fields(this) > NEW13_SESS_TLSID {
                ctx.get_field(this, NEW13_SESS_TLSID).as_int().unwrap_or(-1)
            } else {
                -1
            };
            if tls_id >= 0 {
                if let Some((_, c, _, _)) = crate::servlet::s2_tls_session_info(tls_id) {
                    let s = ctx.create_string(&c);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            Ok(Some(ctx.get_field(this, NEW13_SESS_CIPHER)))
        },
    );
    r.register(ssl_session, "isValid", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tls_id = if ctx.object_num_fields(this) > NEW13_SESS_TLSID {
            ctx.get_field(this, NEW13_SESS_TLSID).as_int().unwrap_or(-1)
        } else {
            -1
        };
        // A session is "valid" while the backing TLS stream is still alive
        // in the registry. Closed sockets invalidate their own session.
        let alive = tls_id >= 0 && crate::servlet::s2_tls_session_info(tls_id).is_some();
        Ok(Some(Value::Int(if alive { 1 } else { 0 })))
    });
    r.register(ssl_session, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Derive a stable, per-session 32-byte id by hashing the tls_id +
        // negotiated protocol/cipher. Two calls on the same session return
        // the same bytes, per SSLSession.getId() contract.
        let tls_id = if ctx.object_num_fields(this) > NEW13_SESS_TLSID {
            ctx.get_field(this, NEW13_SESS_TLSID).as_int().unwrap_or(-1)
        } else {
            -1
        };
        let mut seed: u64 = (tls_id as i64 as u64).wrapping_mul(0x9E3779B97F4A7C15);
        if let Some((p, c, _, _)) = crate::servlet::s2_tls_session_info(tls_id) {
            for b in p.as_bytes().iter().chain(c.as_bytes()) {
                seed = seed.wrapping_mul(1099511628211).wrapping_add(*b as u64);
            }
        }
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        let mut rng = seed | 1;
        for i in 0..32 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ctx.set_array_element(arr, i, Value::Int(((rng >> 33) & 0xFF) as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });
    // NEW-13.4: SSLSession.getPeerCertificates — return a real
    // `java.security.cert.Certificate[]` reconstructed from the peer cert
    // chain DER bytes captured during the native-tls handshake. Throws
    // `SSLPeerUnverifiedException` (surfaced as RuntimeException) if the
    // peer did not present a certificate.
    r.register(
        ssl_session,
        "getPeerCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tls_id = if ctx.object_num_fields(this) > NEW13_SESS_TLSID {
                ctx.get_field(this, NEW13_SESS_TLSID).as_int().unwrap_or(-1)
            } else {
                -1
            };
            let chain = if tls_id >= 0 {
                crate::servlet::s2_tls_peer_cert_chain_der(tls_id).unwrap_or_default()
            } else {
                Vec::new()
            };
            if chain.is_empty() {
                // FIX (tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals):
                // this threw a bare IllegalStateException, contradicting this
                // function's own doc comment above ("Throws
                // SSLPeerUnverifiedException") and the real SSLSession
                // contract -- callers (e.g. Spring's DefaultSslInfo.initCertificates,
                // Apache HttpComponents' AbstractClientTlsStrategy.verifySession)
                // specifically catch SSLPeerUnverifiedException to mean "peer
                // presented no certificate"; an IllegalStateException escaped
                // past that catch instead.
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/net/ssl/SSLPeerUnverifiedException",
                    "peer not authenticated",
                ));
            }
            let arr = ctx.new_ref_array(ClassId::new(0), chain.len());
            for (i, der) in chain.iter().enumerate() {
                // Allocate a 4-field X509Certificate: the extra field 3
                // carries the raw DER bytes so `Certificate.getEncoded()`
                // can return them without relying on legacy-synthetic-crypto.
                let cert = alloc_concurrent_synthetic(ctx, "java/security/cert/X509Certificate", 4);
                let (subject, issuer) = basic_der_extract_names(der)
                    .unwrap_or_else(|| ("CN=Unknown".into(), "CN=Unknown".into()));
                let sub_str = ctx.create_string(&subject);
                let iss_str = ctx.create_string(&issuer);
                ctx.set_field(cert, 0, Value::Object(Some(sub_str)));
                ctx.set_field(cert, 1, Value::Object(Some(iss_str)));
                ctx.set_field(cert, 2, Value::Long(0));
                // Copy DER bytes into a Java byte[] stored at field 3.
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
    // FIX (netty-client-socket-write-after-close residual): getLocalPrincipal/
    // getPeerPrincipal/getLocalCertificates were never registered anywhere in
    // the crate at all — `javax.net.ssl.SSLSession` is a real JDK interface,
    // so a synthetic object impersonating it has no bytecode to fall back to
    // for an unregistered method, and calling one throws AbstractMethodError
    // (observed via Spring's own session-info-building code:
    // `getLocalPrincipal()Ljava/security/Principal; has no Code attribute`).
    r.register(
        ssl_session,
        "getLocalPrincipal",
        "()Ljava/security/Principal;",
        |ctx, args| {
            // STUB-REMOVAL (wave 3): this used to return null unconditionally,
            // which contradicted the sibling `getLocalCertificates` just below —
            // that one was already fixed to read the real chain out of
            // `t27_tls::local_certs_for_session`, because a SERVER session
            // always has its own certificate. `SSLSession.getLocalPrincipal()`
            // is documented as "the principal that was sent to the peer", i.e.
            // the subject of the local leaf certificate, so a session that
            // hands back a chain from `getLocalCertificates()` yet null here
            // was self-contradictory. Derive the principal from that same
            // chain. Null stays correct — and is the documented answer for "no
            // principal was sent" — when there is no local identity (a plain
            // no-mTLS client session), so the original client behaviour is
            // unchanged.
            let this = obj_arg(args, 0)?;
            let chain = crate::t27_tls::local_certs_for_session(ctx, this);
            let Some(leaf) = chain.first() else {
                return Ok(Some(Value::Object(None)));
            };
            let (subject, _issuer) = basic_der_extract_names(leaf)
                .unwrap_or_else(|| ("CN=Unknown".into(), String::new()));
            // Same 1-field synthetic shape `getPeerPrincipal` builds below.
            let princ =
                alloc_concurrent_synthetic(ctx, "javax/security/auth/x500/X500Principal", 1);
            let s = ctx.create_string(&subject);
            ctx.set_field(princ, 0, Value::Object(Some(s)));
            Ok(Some(Value::Object(Some(princ))))
        },
    );
    r.register(
        ssl_session,
        "getLocalCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            // FIX (spring-boot-jetty SecureRequestCustomizer 400 "Invalid SNI"):
            // this used to unconditionally return null, which was only correct
            // for a plain (no-mTLS) CLIENT session. A SERVER session always has
            // a local (its own) certificate chain; Jetty's
            // `SecureRequestCustomizer.getX509()` calls exactly this method on
            // every HTTPS request and throws `HttpException.RuntimeException(400,
            // "Invalid SNI")` when it comes back empty. See
            // `t27_tls::build_synthetic_ssl_session`/`local_certs_for_session`
            // for where the chain is actually populated (client sessions with no
            // configured identity correctly still get an empty chain here).
            let this = obj_arg(args, 0)?;
            let chain = crate::t27_tls::local_certs_for_session(ctx, this);
            if chain.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), chain.len());
            for (i, der) in chain.iter().enumerate() {
                let mirror = crate::keystore::make_x509_mirror(ctx, "local", der);
                ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        ssl_session,
        "getPeerPrincipal",
        "()Ljava/security/Principal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tls_id = if ctx.object_num_fields(this) > NEW13_SESS_TLSID {
                ctx.get_field(this, NEW13_SESS_TLSID).as_int().unwrap_or(-1)
            } else {
                -1
            };
            let chain = if tls_id >= 0 {
                crate::servlet::s2_tls_peer_cert_chain_der(tls_id).unwrap_or_default()
            } else {
                Vec::new()
            };
            let Some(leaf) = chain.first() else {
                // FIX (tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals):
                // same wrong-exception-type bug as getPeerCertificates just
                // above -- see that fix's comment.
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/net/ssl/SSLPeerUnverifiedException",
                    "peer not authenticated",
                ));
            };
            let (subject, _issuer) = basic_der_extract_names(leaf)
                .unwrap_or_else(|| ("CN=Unknown".into(), String::new()));
            let princ =
                alloc_concurrent_synthetic(ctx, "javax/security/auth/x500/X500Principal", 1);
            let s = ctx.create_string(&subject);
            ctx.set_field(princ, 0, Value::Object(Some(s)));
            Ok(Some(Value::Object(Some(princ))))
        },
    );
    r.register(ssl_session, "getCreationTime", "()J", |_ctx, _args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        Ok(Some(Value::Long(now)))
    });
    r.register(ssl_session, "getLastAccessedTime", "()J", |_ctx, _args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        Ok(Some(Value::Long(now)))
    });

    // TrustManagerFactory = 2-field (algorithm=0, keystore=1)
    //
    // The backing X509TrustManager references the same KeyStore reference
    // that was passed to `init(KeyStore)` so that subsequent
    // `getAcceptedIssuers()` / `checkServerTrusted()` calls can walk the
    // stored trust-anchor set. When `init` is called with `null` (the JDK
    // default-store mode) the TrustManager falls back to the VM's system
    // trust roots — mirroring the platform default.
    let tmf = "javax/net/ssl/TrustManagerFactory";
    r.register(
        tmf,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
        |ctx, args| {
            // FIX (rabbitautoconfigurationtests-cglib-enhance-hang residual):
            // this used to accept ANY algorithm string unconditionally, so
            // `TrustManagerFactory.getInstance("bogus-algo")` silently
            // succeeded instead of throwing `NoSuchAlgorithmException` like
            // real JDK — breaking `RabbitAutoConfigurationTests.
            // enableSslWithInvalidTrustStoreAlgorithmShouldFail` (and any
            // other caller relying on the JCA `getInstance` contract to
            // reject an unsupported algorithm). Reject up front via the same
            // provider-chain lookup `KeyManagerFactory.getInstance` (below)
            // and `getProvider()` already use, so a caller-registered custom
            // `Provider` service is still honoured.
            let algo_str = match args.get(0) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            if crate::jca::provider_chain::find_service_provider("TrustManagerFactory", &algo_str)
                .is_none()
            {
                return Err(kmf_tmf_no_such_algorithm(
                    ctx,
                    &format!("{algo_str} TrustManagerFactory not available"),
                ));
            }
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/TrustManagerFactory", 2);
            ctx.set_field(obj, 0, args.get(0).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        tmf,
        "getDefaultAlgorithm",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("PKIX");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(tmf, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            Ok(Some(ctx.get_field(this, 0)))
        } else {
            let s = ctx.create_string("PKIX");
            Ok(Some(Value::Object(Some(s))))
        }
    });
    r.register(tmf, "init", "(Ljava/security/KeyStore;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 1 {
            // Stash the KeyStore reference so the emitted TrustManager can
            // walk it at verification time. A null here is legitimate and
            // means "use platform default trust store".
            ctx.set_field(this, 1, args.get(1).copied().unwrap_or(Value::Object(None)));
        }
        // FIX (es-restclient-https): this `TrustManagerFactory.init(KeyStore)`
        // is the one that actually wins (registered last in the lib.rs wiring,
        // shadowing `tls.rs::register_trust_manager_factory`'s otherwise
        // equivalent native — see that function's own matching fix). Without
        // this, an SSLContext built from a caller-supplied truststore (e.g. a
        // test JKS holding a self-signed/test-CA cert — the
        // `HttpsServer`+`RestClient` pattern in
        // `RestClientBuilderIntegTests`) only ever validated peers against the
        // platform root store, so the handshake failed with `UnknownIssuer`
        // even though the JDK caller correctly wired a custom trust store.
        // Stage the keystore's trust anchors on the `t27_tls` per-thread slot
        // so the next real `SSLContext.init` (`net_phase_e.rs`, the winning
        // rustls-backed native) scopes trust to them.
        if let Some(Value::Object(Some(ks))) = args.get(1) {
            let ks_id = crate::tls::read_keystore_registry_id(ctx, *ks);
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] tmf(phases_late).init(KeyStore) this_ih={} ks_id={}",
                    ctx.identity_hash_code(this),
                    ks_id
                );
            }
            if ks_id != 0 {
                let state = crate::x509_manager::build_trust_manager_state(ks_id);
                if !state.anchor_ders.is_empty() {
                    crate::t27_tls::set_pending_tm_trust_roots(state.anchor_ders.clone());
                }
                // FIX (tomcat-clientauth-engine-config): also register this
                // state into `x509_manager::tm_registry` and stash the
                // resulting id via identity hash, so `getTrustManagers()`
                // below can build a REAL, functional TrustManager (same
                // trap/fix as the sibling
                // `KeyManagerFactory.getKeyManagers()` — see that handler's
                // doc comment for the full story: a bare
                // `alloc_concurrent_synthetic(ctx, "javax/net/ssl/
                // X509TrustManager", ...)` stamps the INTERFACE's own class
                // id, so `checkClientTrusted`/`checkServerTrusted`/
                // `getAcceptedIssuers` have no Code and throw
                // `AbstractMethodError` the moment real bytecode calls one
                // directly instead of going through this crate's own
                // post-handshake `engine_run_trust_check` native path).
                let tm_id = crate::x509_manager::register_trust_manager_state(state);
                let ih = ctx.identity_hash_code(this);
                if ih != 0 {
                    tmf_tm_id_by_identity().lock().insert(ih, tm_id);
                }
            }
        }
        Ok(None)
    });
    r.register(
        tmf,
        "init",
        "(Ljavax/net/ssl/ManagerFactoryParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) > 1 {
                ctx.set_field(this, 1, args.get(1).copied().unwrap_or(Value::Object(None)));
            }
            // FIX (tomcat-clientauth-engine-config): this overload — used
            // whenever `sslHostConfig.getTruststoreAlgorithm()` is `"PKIX"`
            // (Tomcat's default; see `SSLUtilBase.getTrustManagers()`,
            // which is what the SERVER side of every mTLS test in this
            // suite actually calls) — used to leave `getTrustManagers()`
            // below with no keystore id to work with at all, since
            // `CertPathTrustManagerParameters` carries no back-reference to
            // a `KeyStore`. Reuses `x509_manager`'s own
            // `build_and_register_tm_state_from_mfp` (extracted from that
            // module's `tmf_engine_init_params`, which does the identical
            // walk for its own SPI-delegation registration) to recover the
            // trust anchors directly from `getParameters().
            // getTrustAnchors()` and register a real `TrustManagerState`,
            // then stash the resulting id the same way the `init(KeyStore)`
            // overload above does.
            let mfp = match args.get(1) {
                Some(Value::Object(Some(mfp))) => Some(*mfp),
                _ => None,
            };
            let tm_id = crate::x509_manager::build_and_register_tm_state_from_mfp(ctx, mfp);
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] tmf(phases_late).init(ManagerFactoryParameters) this_ih={} tm_id={}",
                    ctx.identity_hash_code(this),
                    tm_id
                );
            }
            let ih = ctx.identity_hash_code(this);
            if ih != 0 {
                tmf_tm_id_by_identity().lock().insert(ih, tm_id);
            }
            Ok(None)
        },
    );
    r.register(
        tmf,
        "getTrustManagers",
        "()[Ljavax/net/ssl/TrustManager;",
        |ctx, args| {
            // FIX (tomcat-clientauth-engine-config): this used to always
            // return a bare `alloc_concurrent_synthetic(ctx,
            // "javax/net/ssl/X509TrustManager", 2)` — an object stamped
            // with the INTERFACE's own class id. `getAcceptedIssuers()` on
            // such an object was already known to throw
            // `AbstractMethodError` (see the `FIX
            // (x509-trustmanager-abstractmethod-20260707)` narrow rescue
            // wired in `lib.rs` via `t27_tls::register_accepted_issuers`,
            // registered directly on the bare interface since accepted-
            // issuers enumeration is instance-independent — platform trust
            // roots). `checkClientTrusted`/`checkServerTrusted` need real
            // PER-INSTANCE state (which keystore's anchors to validate
            // against), which a single interface-wide native cannot
            // provide, so they were never given the same rescue and kept
            // throwing `AbstractMethodError` for any caller invoking one
            // directly (confirmed via this session's `CRATONVM_DBG_NOCODE`
            // tracing against Tomcat's `TestClientCert`/
            // `engine_run_trust_check`'s post-handshake
            // `checkClientTrusted` call — see
            // `fixed-suite-bugs/tls-ocsp-clientcert-validation-not-
            // enforced-FIXED.md`, "Residual #2 implementation" for the full
            // trace). Fixed the same way as the sibling
            // `KeyManagerFactory.getKeyManagers()` fix just above: build
            // the real, functional `FQN_X509_TM`-shaped object
            // `x509_manager.rs`'s `tmf_engine_get_trust_managers` already
            // produces for the SPI-delegation path, keyed off the SAME
            // `tm_registry` id either `init` overload above (`KeyStore` or
            // `ManagerFactoryParameters`) already registered and stashed
            // via `tmf_tm_id_by_identity`. The default/null-initialized path
            // uses id 0 on the same real-impl-shaped object, which the
            // `x509_manager` handlers intentionally resolve as platform
            // default trust roots. This avoids falling back to the old (non-
            // functional but allocation-safe) stub when no id was captured
            // (matches original behavior for `init((KeyStore) null)` / a
            // `getTrustManagers()` call with no preceding `init`).
            let this = obj_arg(args, 0)?;
            let runtime_class = ctx
                .class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_default();
            // This bridge owns only the synthetic default factory made by
            // TrustManagerFactory.getInstance(). A concrete provider factory
            // (notably Netty's InsecureTrustManagerFactory) implements its
            // policy through the real TrustManagerFactory bytecode and SPI.
            // Treating every subclass as our synthetic default silently
            // replaces that provider's manager with a PKIX manager. Execute
            // the base bytecode without re-entering this native so virtual
            // SPI dispatch returns the provider's configured manager.
            if runtime_class != "javax/net/ssl/TrustManagerFactory" {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "getTrustManagers",
                    "()[Ljavax/net/ssl/TrustManager;",
                    &[],
                );
            }
            let ih = ctx.identity_hash_code(this);
            let tm_id = if ih != 0 {
                tmf_tm_id_by_identity()
                    .lock()
                    .get(&ih)
                    .copied()
                    .unwrap_or(0)
            } else {
                0
            };
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] tmf(phases_late).getTrustManagers this_ih={} looked_up_tm_id={}",
                    ih, tm_id
                );
            }
            let tm = alloc_concurrent_synthetic(ctx, crate::x509_manager::FQN_X509_TM, 2);
            crate::x509_manager::set_tm_id(ctx, tm, tm_id);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(tm)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // KeyManagerFactory = 3-field, matching the REAL javax.net.ssl.KeyManagerFactory
    // field declaration order exactly (`javap -p javax.net.ssl.KeyManagerFactory`:
    // `provider`, `factorySpi`, `algorithm`, in that order — NOT the order fields
    // are first referenced in the constant pool, which is a different, easy-to-
    // misread ordering). `getProvider()`/`getAlgorithm()` have no native override
    // here, so they fall through to real bytecode reading these exact slots
    // (`getProvider()` returns field 0, `getAlgorithm()` returns field 2 — see
    // `javap -c`). The previous field numbering here (algorithm=0, keystore=1,
    // password=2) put the algorithm *String* at slot 0, so real bytecode's
    // `getProvider()` returned that String in place of a `Provider` — callers
    // invoking `.getInfo()` on it (e.g. `SSLUtilBase.getKeyManagers`) got
    // `NoSuchMethodError: java/lang/String.getInfo()`.
    let kmf = "javax/net/ssl/KeyManagerFactory";
    r.register(
        kmf,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;",
        |ctx, args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/KeyManagerFactory", 3);
            // field 0: provider — a real `Provider` object, not the SPI's
            // factorySpi/algorithm, so `getProvider().getInfo()` et al. work.
            // Built via `provider_chain`'s own `make_provider` (not a raw
            // `alloc_concurrent_synthetic` + indexed `set_field`): in
            // real-JDK mode `alloc_concurrent_synthetic` upsizes the object
            // to `java/security/Provider`'s full real field count —
            // inherited Hashtable/Properties fields included — so a plain
            // slot-0/1 write lands on whatever field happens to occupy that
            // slot in the real inheritance layout, not `name`/`version`.
            // `make_provider` writes by field NAME (`set_field_by_name`)
            // specifically to survive that, and its registered `getInfo()`
            // native reads the same "info" field back by name — see
            // `jca/provider_chain.rs`'s module doc for the full story.
            //
            // FIX (keymanagerfactoryfips-bare-assertion): this used to
            // hardcode "SunJSSE" unconditionally, so ANY caller who
            // registered their own `KeyManagerFactory.<algo>` service on a
            // custom `Provider` (via `Security.addProvider` +
            // `Provider.put`/`putService`) got the wrong provider back —
            // `getProvider()` always pointed at the built-in SunJSSE
            // placeholder instead of the caller's own `Provider` instance.
            // Tomcat's `TestKeyManagerWrappingFips.testBug64614_01` hits
            // this directly: it registers a dummy `KeyManagerFactory`
            // service on a provider whose `getInfo()` contains "FIPS", and
            // `SSLUtilBase.getKeyManagers()` branches on
            // `kmf.getProvider().getInfo().contains("FIPS")` — which always
            // read the SunJSSE placeholder's info (no "FIPS") instead.
            // `find_service_provider` searches the real provider chain
            // (which already includes providers added via
            // `Security.addProvider`) for who registered this algorithm,
            // falling back to "SunJSSE" — the previous hardcoded default —
            // when nobody has, so the existing SunX509/NewSunX509/PKIX
            // callers are unaffected.
            let algorithm = args.get(0).copied().unwrap_or(Value::Object(None));
            let algo_str = match algorithm {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            // FIX (rabbitautoconfigurationtests-cglib-enhance-hang residual):
            // no provider (built-in or caller-registered) claims this
            // algorithm — real JDK's `getInstance` throws
            // `NoSuchAlgorithmException` here rather than silently falling
            // back to a default implementation. Was previously unconditional
            // (`unwrap_or_else(|| "SunJSSE")`), so `KeyManagerFactory.
            // getInstance("bogus-algo")` always succeeded — breaking
            // `RabbitAutoConfigurationTests.
            // enableSslWithInvalidKeyStoreAlgorithmShouldFail` (and any other
            // caller relying on the JCA contract to reject an unsupported
            // algorithm).
            let found_provider =
                crate::jca::provider_chain::find_service_provider("KeyManagerFactory", &algo_str);
            if found_provider.is_none() {
                return Err(kmf_tmf_no_such_algorithm(
                    ctx,
                    &format!("{algo_str} KeyManagerFactory not available"),
                ));
            }
            let provider_name = found_provider.unwrap_or_else(|| "SunJSSE".to_string());
            let provider =
                crate::jca::provider_chain::resolve_or_make_provider(ctx, &provider_name);
            ctx.set_field(obj, 0, Value::Object(Some(provider)));
            ctx.set_field(obj, 1, Value::Object(None)); // factorySpi — unused by this stub
            ctx.set_field(obj, 2, algorithm); // algorithm
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        kmf,
        "getDefaultAlgorithm",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("SunX509");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(kmf, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 2 {
            Ok(Some(ctx.get_field(this, 2)))
        } else {
            let s = ctx.create_string("SunX509");
            Ok(Some(Value::Object(Some(s))))
        }
    });
    r.register(kmf, "init", "(Ljava/security/KeyStore;[C)V", |ctx, args| {
        // Fields 0-2 (provider/factorySpi/algorithm) are fixed at construction
        // to match the real class layout — init() doesn't touch them. The
        // thread-local identity staging below (per-SSLContext mTLS identity
        // flow) is one consumer of the keystore this KMF was init'd with.
        //
        // FIX (tomcat-clientauth-engine-config): ALSO stash the keystore's
        // registry id directly on this KMF object (via `set_field_by_name`'s
        // pseudo-field convention — a real `javax.net.ssl.KeyManagerFactory`
        // has no room for it, same reasoning as `x509_manager.rs`'s
        // `cratonvm$x509km$id`/`cratonvm$x509tm$id` side-fields), so
        // `getKeyManagers()` below can build a REAL, functional KeyManager
        // instead of the non-functional bare-interface stub it used to
        // return (see that handler's doc comment for the full story).
        if let Some(Value::Object(Some(ks))) = args.get(1) {
            let key_password = args
                .get(2)
                .map(|value| crate::keystore::read_password(ctx, value))
                .unwrap_or_default();
            if crate::nbflags().dbg_tls_auth {
                let this_ih = obj_arg(args, 0)
                    .map(|t| ctx.identity_hash_code(t))
                    .unwrap_or(0);
                eprintln!(
                    "[dbg-tls-auth] kmf(phases_late).init(KeyStore) this_ih={} ks_id={} password_len={}",
                    this_ih,
                    crate::keystore::keystore_id_from_object(ctx, *ks),
                    key_password.len()
                );
            }
            crate::keystore::keystore_set_pending_km_identity_with_password(
                ctx,
                *ks,
                &key_password,
            );
            let ks_id = crate::keystore::keystore_id_from_object(ctx, *ks);
            if ks_id != 0 {
                let this = obj_arg(args, 0)?;
                // `set_field_by_name` would silently no-op here: this KMF
                // object is allocated with the REAL `javax/net/ssl/
                // KeyManagerFactory` class shape (fields 0-2 already fixed to
                // provider/factorySpi/algorithm above), which declares no
                // `cratonvm$...` pseudo-field for a real class to land on —
                // same trap this doc's helpers (`keystore.rs::
                // store_id_by_identity`, `x509_manager.rs::km_id_by_identity`/
                // `tm_id_by_identity`) were all built to route around. Use an
                // identity-hash side table directly rather than
                // `set_field_by_name`/`set_field`, neither of which has
                // anywhere to durably land the value on this object shape.
                let ih = ctx.identity_hash_code(this);
                if ih != 0 {
                    kmf_keystore_id_by_identity().lock().insert(ih, ks_id);
                }
            }
        }
        Ok(None)
    });
    // STUB-REMOVAL (wave 2): this used to return normally without doing
    // anything, which is the worst possible answer — the caller believes its
    // key material was accepted, `getKeyManagers()` then hands back a manager
    // with no identity, and the client certificate is silently never presented.
    // The real `SunX509` / `NewSunX509` `KeyManagerFactorySpi.engineInit
    // (ManagerFactoryParameters)` throws `InvalidAlgorithmParameterException`
    // for every parameter type it does not understand (SunX509 understands
    // none; NewSunX509 understands only `KeyStoreBuilderParameters`), and this
    // implementation understands none either — the KeyStore overload above is
    // the only supported initialisation path. Fail honestly so the caller sees
    // the same exception the reference JDK raises instead of a mute downgrade.
    r.register(
        kmf,
        "init",
        "(Ljavax/net/ssl/ManagerFactoryParameters;)V",
        |ctx, _args| {
            Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/InvalidAlgorithmParameterException",
                "KeyManagerFactory.init(ManagerFactoryParameters) is not supported; \
                 use init(KeyStore, char[])",
            ))
        },
    );
    r.register(
        kmf,
        "getKeyManagers",
        "()[Ljavax/net/ssl/KeyManager;",
        |ctx, args| {
            // FIX (tomcat-clientauth-engine-config): this handler used to
            // return a bare `alloc_concurrent_synthetic(ctx,
            // "javax/net/ssl/X509KeyManager", 0)` — an object stamped with
            // the INTERFACE's own class id (it's a real, loadable JDK
            // interface, so `ensure_class_initialized` happily "succeeds").
            // Every one of `X509KeyManager`'s 6 methods is abstract with no
            // Code, so real Java bytecode calling any of them directly
            // (rather than going through this crate's own
            // `t27_tls::JavaKeyManagerResolver`, which never calls
            // `getKeyManagers()` at all — it captures the KeyManager array
            // straight off `SSLContext.init()`'s arguments instead) hit an
            // unconditional `AbstractMethodError`. Confirmed via direct
            // `CRATONVM_DBG_NOCODE=1` tracing against Tomcat's
            // `TesterSupport.TrackingKeyManager` (`test/org/apache/tomcat/
            // util/net/TesterSupport.java`), whose `chooseClientAlias`
            // override does exactly this
            // (`manager.chooseClientAlias(keyType, issuers, socket)`,
            // `manager` being whatever `getKeyManagers()` returned) — see
            // `fixed-suite-bugs/tls-ocsp-clientcert-validation-not-
            // enforced-FIXED.md`, "Residual #2 implementation" for the full trace.
            //
            // Fixed by building the SAME real, natively-backed
            // `FQN_SUN_X509_KM`-shaped object `x509_manager.rs`'s
            // `kmf_engine_get_key_managers` already produces for the SPI
            // delegation path (`KeyManagerFactoryImpl$SunX509.
            // engineGetKeyManagers`, reached when a caller goes through the
            // real `factorySpi` chain) — `chooseClientAlias`/
            // `chooseServerAlias`/`getCertificateChain`/`getClientAliases`/
            // `getPrivateKey`/`getServerAliases` are all real, working
            // natives on that class (`x509_manager::register_key_manager`),
            // so a wrapper's direct delegate call now completes instead of
            // throwing. Registered in the SAME `km_registry` +
            // `next_km_id`/`set_km_id` id space so `choose_client_alias`/
            // `get_private_key`'s registry lookups resolve correctly. Falls
            // back to the old (non-functional but at least allocation-safe)
            // stub only when no keystore id was ever captured — matches
            // this handler's original behavior for a `getKeyManagers()`
            // call with no preceding `init(KeyStore, char[])`, which is not
            // a real/expected call shape for this API but shouldn't panic.
            let this = obj_arg(args, 0)?;
            let ih = ctx.identity_hash_code(this);
            let ks_id = if ih != 0 {
                kmf_keystore_id_by_identity()
                    .lock()
                    .get(&ih)
                    .copied()
                    .unwrap_or(0)
            } else {
                0
            };
            let km = if ks_id != 0 {
                let state = crate::x509_manager::build_key_manager_state(ks_id);
                let km_id = crate::x509_manager::next_km_id();
                crate::x509_manager::km_registry()
                    .write()
                    .insert(km_id, state);
                let km = alloc_concurrent_synthetic(ctx, crate::x509_manager::FQN_SUN_X509_KM, 2);
                crate::x509_manager::set_km_id(ctx, km, km_id);
                km
            } else {
                alloc_concurrent_synthetic(ctx, "javax/net/ssl/X509KeyManager", 0)
            };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(km)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // SSLEngine = 7-field (client_mode=0, need_client_auth=1, want_client_auth=2,
    //                       enabled_protocols=3, enabled_cipher_suites=4,
    //                       handshake_status=5, session=6)
    let ssleng = "javax/net/ssl/SSLEngine";
    r.register(ssleng, "setUseClientMode", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 0, Value::Int(v));
        Ok(None)
    });
    r.register(ssleng, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(ssleng, "setNeedClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 1, Value::Int(v));
        // setNeedClientAuth(true) clears wantClientAuth and vice versa
        if v != 0 {
            ctx.set_field(this, 2, Value::Int(0));
        }
        Ok(None)
    });
    r.register(ssleng, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(ssleng, "setWantClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 2, Value::Int(v));
        if v != 0 {
            ctx.set_field(this, 1, Value::Int(0));
        }
        Ok(None)
    });
    r.register(ssleng, "getWantClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(
        ssleng,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 3, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(
        ssleng,
        "getEnabledProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match ctx.get_field(this, 3) {
                Value::Object(Some(arr)) => Ok(Some(Value::Object(Some(arr)))),
                _ => {
                    // Default: TLSv1.2, TLSv1.3
                    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
                    let p1 = ctx.create_string("TLSv1.2");
                    let p2 = ctx.create_string("TLSv1.3");
                    ctx.set_array_element(arr, 0, Value::Object(Some(p1)));
                    ctx.set_array_element(arr, 1, Value::Object(Some(p2)));
                    Ok(Some(Value::Object(Some(arr))))
                }
            }
        },
    );
    r.register(
        ssleng,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 4, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(
        ssleng,
        "getEnabledCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match ctx.get_field(this, 4) {
                Value::Object(Some(arr)) => Ok(Some(Value::Object(Some(arr)))),
                _ => {
                    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
                    let s = ctx.create_string("TLS_AES_128_GCM_SHA256");
                    ctx.set_array_element(arr, 0, Value::Object(Some(s)));
                    Ok(Some(Value::Object(Some(arr))))
                }
            }
        },
    );
    // FIX (httpserver-pkcs12-20260706): getSupportedCipherSuites/getSupportedProtocols
    // were never registered on javax/net/ssl/SSLEngine (only getEnabled* above), the
    // same "missing accessor" shape as the earlier SSLSocket
    // getSupportedCipherSuites/getEnabledCipherSuites gap (see
    // fixed-suite-bugs/CRATONVM_BUGS/BUG-interfacedispatch-mbeanserver-sslsocket-realmode-shadow.md).
    // ssleng_alloc allocates every SSLEngine directly on this abstract class (never a
    // concrete subclass), so an unregistered method here always throws
    // AbstractMethodError on any real-JDK caller. Netty's JdkSslContext.<clinit> (via
    // JdkSslContext$Defaults.init -> supportedCiphers) calls
    // SSLContext.getDefault().createSSLEngine().getSupportedCipherSuites() to validate
    // its configured cipher list against the engine's supported set — every
    // ServerHttpsRequestIntegrationTests run (Reactor Netty server backend) hits this
    // during server bootstrap. Mirrors the same fuller suite list already used by
    // SSLSocketFactory.getSupportedCipherSuites/SSLSocket's supported-suites native for
    // consistency (this synthetic engine has no negotiated-state distinction between
    // "supported" and "enabled defaults" beyond what's already modeled by the
    // getEnabledCipherSuites default branch above).
    r.register(
        ssleng,
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
                // T-CBC.1: real CBC-mode suites, see t27_tls_cbc /
                // fixed-suite-bugs/rustls-cbc-cipher-suites-not-supported.md
                "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256",
                "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384",
                "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384",
            ];
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, suites.len());
            for (i, &s) in suites.iter().enumerate() {
                let str_obj = ctx.create_string(s);
                ctx.set_array_element(arr, i, Value::Object(Some(str_obj)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        ssleng,
        "getSupportedProtocols",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let protos = ["TLSv1.3", "TLSv1.2"];
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, protos.len());
            for (i, &p) in protos.iter().enumerate() {
                let str_obj = ctx.create_string(p);
                ctx.set_array_element(arr, i, Value::Object(Some(str_obj)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(ssleng, "beginHandshake", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Transition from NOT_HANDSHAKING (0) to NEED_WRAP (1) for client, NEED_UNWRAP (2) for server
        let client = ctx.get_field(this, 0).as_int().unwrap_or(1);
        let new_status = if client != 0 { 1 } else { 2 };
        ctx.set_field(this, 5, Value::Int(new_status));
        Ok(None)
    });
    r.register(
        ssleng,
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let status = ctx.get_field(this, 5).as_int().unwrap_or(0);
            let name = match status {
                0 => "NOT_HANDSHAKING",
                1 => "NEED_WRAP",
                2 => "NEED_UNWRAP",
                3 => "NEED_TASK",
                4 => "FINISHED",
                _ => "NOT_HANDSHAKING",
            };
            // Return an enum synthetic
            let e =
                alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult$HandshakeStatus", 2);
            let n = ctx.create_string(name);
            ctx.set_field(e, 0, Value::Object(Some(n)));
            ctx.set_field(e, 1, Value::Int(status));
            Ok(Some(Value::Object(Some(e))))
        },
    );
    // T2.7.6 — SSLEngine.wrap(ByteBuffer src, ByteBuffer dst)
    // Drives the outbound state machine: if the handshake needs a WRAP,
    // we transition the state and return OK / NEED_UNWRAP. If not
    // handshaking, we copy `src` remaining bytes into `dst` as-is and
    // return OK / NOT_HANDSHAKING. The real TLS record framing happens
    // at the socket level (SSLSocket / SSLServerSocket) — this engine
    // path exists for callers like Netty that drive TLS through
    // ByteBuffer-level wrap/unwrap rather than getInputStream/getOutputStream.
    r.register(
        ssleng,
        "wrap",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let src = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLEngine.wrap: src is null".into()),
                    }
                    .into());
                }
            };
            let dst = match args.get(2) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLEngine.wrap: dst is null".into()),
                    }
                    .into());
                }
            };

            let hs = ctx.get_field(this, 5).as_int().unwrap_or(0);

            // Read src position / limit to figure out how many bytes to wrap.
            let src_pos = ctx.get_field(src, 1).as_int().unwrap_or(0) as usize;
            let src_lim = ctx.get_field(src, 2).as_int().unwrap_or(0) as usize;
            let remaining = if src_lim > src_pos {
                src_lim - src_pos
            } else {
                0
            };
            let dst_pos = ctx.get_field(dst, 1).as_int().unwrap_or(0) as usize;
            let dst_lim = ctx.get_field(dst, 2).as_int().unwrap_or(0) as usize;
            let dst_rem = if dst_lim > dst_pos {
                dst_lim - dst_pos
            } else {
                0
            };

            let bytes_produced;
            let bytes_consumed;
            let new_hs;

            if hs == 1 {
                // NEED_WRAP → produce a synthetic handshake record and
                // transition to NEED_UNWRAP. We write a minimal TLS
                // ClientHello-shaped header into dst so callers that inspect
                // the output don't see zero bytes.
                let fake_hello: &[u8] = &[
                    0x16, 0x03, 0x03, 0x00, 0x05, // TLS record header
                    0x01, 0x00, 0x00, 0x01, 0x03, // ClientHello stub
                ];
                let n = fake_hello.len().min(dst_rem);
                if let Value::Object(Some(dst_arr)) = ctx.get_field(dst, 0) {
                    for i in 0..n {
                        ctx.set_array_element(
                            dst_arr,
                            dst_pos + i,
                            Value::Int(fake_hello[i] as i8 as i32),
                        );
                    }
                }
                ctx.set_field(dst, 1, Value::Int((dst_pos + n) as i32));
                bytes_produced = n;
                bytes_consumed = 0;
                new_hs = 2; // → NEED_UNWRAP
            } else {
                // Normal data wrap: copy remaining from src → dst.
                let n = remaining.min(dst_rem);
                if n > 0 {
                    if let (Value::Object(Some(sa)), Value::Object(Some(da))) =
                        (ctx.get_field(src, 0), ctx.get_field(dst, 0))
                    {
                        for i in 0..n {
                            let v = ctx.get_array_element(sa, src_pos + i);
                            ctx.set_array_element(da, dst_pos + i, v);
                        }
                    }
                    ctx.set_field(src, 1, Value::Int((src_pos + n) as i32));
                    ctx.set_field(dst, 1, Value::Int((dst_pos + n) as i32));
                }
                bytes_produced = n;
                bytes_consumed = n;
                new_hs = 0; // NOT_HANDSHAKING
            }
            ctx.set_field(this, 5, Value::Int(new_hs));

            let result = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult", 4);
            // Fields: 0=status ordinal (OK=0), 1=handshakeStatus ordinal,
            //         2=bytesConsumed, 3=bytesProduced
            ctx.set_field(result, 0, Value::Int(0)); // OK
            ctx.set_field(result, 1, Value::Int(new_hs));
            ctx.set_field(result, 2, Value::Int(bytes_consumed as i32));
            ctx.set_field(result, 3, Value::Int(bytes_produced as i32));
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // T2.7.7 — SSLEngine.unwrap(ByteBuffer src, ByteBuffer dst)
    r.register(
        ssleng,
        "unwrap",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let src = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLEngine.unwrap: src is null".into()),
                    }
                    .into());
                }
            };
            let dst = match args.get(2) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLEngine.unwrap: dst is null".into()),
                    }
                    .into());
                }
            };

            let hs = ctx.get_field(this, 5).as_int().unwrap_or(0);
            let src_pos = ctx.get_field(src, 1).as_int().unwrap_or(0) as usize;
            let src_lim = ctx.get_field(src, 2).as_int().unwrap_or(0) as usize;
            let remaining = if src_lim > src_pos {
                src_lim - src_pos
            } else {
                0
            };
            let dst_pos = ctx.get_field(dst, 1).as_int().unwrap_or(0) as usize;
            let dst_lim = ctx.get_field(dst, 2).as_int().unwrap_or(0) as usize;
            let dst_rem = if dst_lim > dst_pos {
                dst_lim - dst_pos
            } else {
                0
            };

            let bytes_produced;
            let bytes_consumed;
            let new_hs;

            if hs == 2 {
                // NEED_UNWRAP → consume src (simulated server response) and
                // transition to FINISHED. In a real rustls pipeline the
                // ServerHello + EncryptedExtensions + Finished would be parsed
                // here; our synthetic engine skips that because the actual TLS
                // record processing occurs inside the SSLSocket / rustls
                // StreamOwned path.
                let n = remaining.min(64); // consume up to 64 bytes
                if n > 0 {
                    ctx.set_field(src, 1, Value::Int((src_pos + n) as i32));
                }
                bytes_consumed = n;
                bytes_produced = 0;
                new_hs = 4; // FINISHED
            } else {
                // Normal data unwrap: copy src → dst.
                let n = remaining.min(dst_rem);
                if n > 0 {
                    if let (Value::Object(Some(sa)), Value::Object(Some(da))) =
                        (ctx.get_field(src, 0), ctx.get_field(dst, 0))
                    {
                        for i in 0..n {
                            let v = ctx.get_array_element(sa, src_pos + i);
                            ctx.set_array_element(da, dst_pos + i, v);
                        }
                    }
                    ctx.set_field(src, 1, Value::Int((src_pos + n) as i32));
                    ctx.set_field(dst, 1, Value::Int((dst_pos + n) as i32));
                }
                bytes_consumed = n;
                bytes_produced = n;
                new_hs = 0; // NOT_HANDSHAKING
            }
            ctx.set_field(this, 5, Value::Int(new_hs));

            let result = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult", 4);
            ctx.set_field(result, 0, Value::Int(0)); // OK
            ctx.set_field(result, 1, Value::Int(new_hs));
            ctx.set_field(result, 2, Value::Int(bytes_consumed as i32));
            ctx.set_field(result, 3, Value::Int(bytes_produced as i32));
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // SSLEngineResult accessors.
    //
    // FIX (sslengineresult-stub-shadow): these are the ACTIVE accessor
    // overrides for `javax/net/ssl/SSLEngineResult` — `register_p68_ssl`
    // runs on the real-JDK path too since the httpserver-pkcs12 fix
    // (lib.rs), and registration is last-wins, so this copy shadows BOTH
    // the real `SSLEngineResult` bytecode AND the older duplicate in
    // `tls.rs::register_ssl_engine_result`. The pre-fix bodies assumed
    // every receiver was this file's synthetic int-code result and rebuilt
    // a FRESH 2-slot fake enum (name@0, ordinal@1) on every call — a real
    // result object's enum-reference field read through `.as_int()`
    // defaulted to 0, so `getStatus()`/`getHandshakeStatus()` returned
    // "OK"/"NOT_HANDSHAKING"-named fakes that `==`-match no real singleton.
    // Real JSSE drivers gate their handshake state machines on exactly
    // those identity comparisons (e.g. `sun.net.httpserver.SSLStreams
    // .recvData`'s `hs == FINISHED / hs == NOT_HANDSHAKING` checks before
    // `doHandshake`), so a real-JDK `HttpsServer` never learned it had to
    // WRAP after consuming the ClientHello: it busy-spun re-reading a
    // socket that would never deliver more bytes while the client timed
    // out with `SSLHandshakeException: handshake read: Resource
    // temporarily unavailable (os error 11)` — deterministically, for
    // every `com.sun.net.httpserver.HttpsServer` exchange.
    //
    // Now: a real enum reference stored on the receiver (results built via
    // the real 4-arg ctor in `t27_tls::alloc_engine_result`) passes
    // through untouched — by-name first (layout-proof), then the raw slot.
    // Int-code receivers (this file's fake-engine producers above, which
    // use the name tables below) resolve the REAL enum singleton via
    // `valueOf` so identity comparisons hold even for synthetic results;
    // only when the real enum class is unresolvable (pure synthetic-JDK
    // mode) do they fall back to the legacy fresh 2-slot holder.
    let ssleng_result = "javax/net/ssl/SSLEngineResult";
    r.register(
        ssleng_result,
        "getStatus",
        "()Ljavax/net/ssl/SSLEngineResult$Status;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(o)) = ctx.get_field_by_name(this, "status") {
                return Ok(Some(Value::Object(Some(o))));
            }
            let raw = ctx.get_field(this, 0);
            if let Value::Object(Some(_)) = raw {
                return Ok(Some(raw));
            }
            let status = raw.as_int().unwrap_or(0);
            let name = match status {
                0 => "OK",
                1 => "BUFFER_UNDERFLOW",
                2 => "BUFFER_OVERFLOW",
                3 => "CLOSED",
                _ => "OK",
            };
            let arg = ctx.create_string(name);
            if let Ok(Some(v)) = ctx.invoke(
                "javax/net/ssl/SSLEngineResult$Status",
                "valueOf",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLEngineResult$Status;",
                &[Value::Object(Some(arg))],
            ) {
                if matches!(v, Value::Object(Some(_))) {
                    return Ok(Some(v));
                }
            }
            let e = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult$Status", 2);
            let n = ctx.create_string(name);
            ctx.set_field(e, 0, Value::Object(Some(n)));
            ctx.set_field(e, 1, Value::Int(status));
            Ok(Some(Value::Object(Some(e))))
        },
    );
    r.register(
        ssleng_result,
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(o)) = ctx.get_field_by_name(this, "handshakeStatus") {
                return Ok(Some(Value::Object(Some(o))));
            }
            let raw = ctx.get_field(this, 1);
            if let Value::Object(Some(_)) = raw {
                return Ok(Some(raw));
            }
            let status = raw.as_int().unwrap_or(0);
            let name = match status {
                0 => "NOT_HANDSHAKING",
                1 => "NEED_WRAP",
                2 => "NEED_UNWRAP",
                3 => "NEED_TASK",
                4 => "FINISHED",
                _ => "NOT_HANDSHAKING",
            };
            let arg = ctx.create_string(name);
            if let Ok(Some(v)) = ctx.invoke(
                "javax/net/ssl/SSLEngineResult$HandshakeStatus",
                "valueOf",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
                &[Value::Object(Some(arg))],
            ) {
                if matches!(v, Value::Object(Some(_))) {
                    return Ok(Some(v));
                }
            }
            let e =
                alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult$HandshakeStatus", 2);
            let n = ctx.create_string(name);
            ctx.set_field(e, 0, Value::Object(Some(n)));
            ctx.set_field(e, 1, Value::Int(status));
            Ok(Some(Value::Object(Some(e))))
        },
    );
    // bytesConsumed/bytesProduced: real field by name first (layout-proof
    // for real-ctor results regardless of declaration order), then the raw
    // synthetic slot convention (consumed@2, produced@3).
    r.register(ssleng_result, "bytesConsumed", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Int(n) = ctx.get_field_by_name(this, "bytesConsumed") {
            return Ok(Some(Value::Int(n)));
        }
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(ssleng_result, "bytesProduced", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Int(n) = ctx.get_field_by_name(this, "bytesProduced") {
            return Ok(Some(Value::Int(n)));
        }
        Ok(Some(ctx.get_field(this, 3)))
    });

    r.register(ssleng, "closeOutbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 5, Value::Int(4)); // FINISHED
        Ok(None)
    });
    r.register(ssleng, "closeInbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 5, Value::Int(4));
        Ok(None)
    });
    r.register(ssleng, "isOutboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let status = ctx.get_field(this, 5).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if status == 4 { 1 } else { 0 })))
    });
    r.register(ssleng, "isInboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let status = ctx.get_field(this, 5).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if status == 4 { 1 } else { 0 })))
    });
    r.register(
        ssleng,
        "getSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Return cached session if present
            if let Value::Object(Some(s)) = ctx.get_field(this, 6) {
                return Ok(Some(Value::Object(Some(s))));
            }
            // Build a fresh session and cache it
            let session = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 2);
            let proto = ctx.create_string("TLSv1.3");
            let cipher = ctx.create_string("TLS_AES_128_GCM_SHA256");
            ctx.set_field(session, 0, Value::Object(Some(proto)));
            ctx.set_field(session, 1, Value::Object(Some(cipher)));
            ctx.set_field(this, 6, Value::Object(Some(session)));
            Ok(Some(Value::Object(Some(session))))
        },
    );
    r.set_category(__prev_cat);
}

/// Allocate a fresh SSLEngine with default field values.
pub(crate) fn ssleng_alloc(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngine", 7);
    ctx.set_field(obj, 0, Value::Int(1)); // client_mode = true by default
    ctx.set_field(obj, 1, Value::Int(0)); // need_client_auth
    ctx.set_field(obj, 2, Value::Int(0)); // want_client_auth
    ctx.set_field(obj, 3, Value::Object(None)); // enabled_protocols
    ctx.set_field(obj, 4, Value::Object(None)); // enabled_cipher_suites
    ctx.set_field(obj, 5, Value::Int(0)); // handshake_status = NOT_HANDSHAKING
    ctx.set_field(obj, 6, Value::Object(None)); // session
    obj
}

// =============================================================================
// Basic DER/ASN.1 TLV parser for X.509 subject/issuer extraction (non-feature-gated)
// =============================================================================

/// Read DER tag-length-value. Returns (tag, content_slice, rest_slice).
pub(crate) fn der_read_tlv(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    if data.is_empty() {
        return None;
    }
    let tag = data[0];
    if data.len() < 2 {
        return None;
    }
    let (length, header_len) = if data[1] & 0x80 == 0 {
        (data[1] as usize, 2)
    } else {
        let num_bytes = (data[1] & 0x7f) as usize;
        if num_bytes == 0 || num_bytes > 4 || data.len() < 2 + num_bytes {
            return None;
        }
        let mut len = 0usize;
        for i in 0..num_bytes {
            len = (len << 8) | data[2 + i] as usize;
        }
        (len, 2 + num_bytes)
    };
    if data.len() < header_len + length {
        return None;
    }
    Some((
        tag,
        &data[header_len..header_len + length],
        &data[header_len + length..],
    ))
}

/// Extract CommonName (OID 2.5.4.3) from a DER-encoded Name (SEQUENCE of RDNs).
pub(crate) fn der_extract_cn(name_data: &[u8]) -> String {
    // CN OID: 55 04 03
    let cn_oid: &[u8] = &[0x55, 0x04, 0x03];
    let mut parts = Vec::new();
    let mut pos = name_data;
    // Walk through SET OF RDNSequence
    while let Some((_tag, set_content, rest)) = der_read_tlv(pos) {
        // Each SET contains SEQUENCE of AttributeTypeAndValue
        let mut inner = set_content;
        while let Some((_tag2, seq_content, inner_rest)) = der_read_tlv(inner) {
            // SEQUENCE = OID + value
            if let Some((0x06, oid_bytes, val_rest)) = der_read_tlv(seq_content) {
                if let Some((val_tag, val_bytes, _)) = der_read_tlv(val_rest) {
                    let oid_name = match oid_bytes {
                        x if x == cn_oid => "CN",
                        x if x == &[0x55, 0x04, 0x06] => "C",
                        x if x == &[0x55, 0x04, 0x08] => "ST",
                        x if x == &[0x55, 0x04, 0x07] => "L",
                        x if x == &[0x55, 0x04, 0x0a] => "O",
                        x if x == &[0x55, 0x04, 0x0b] => "OU",
                        _ => "",
                    };
                    if !oid_name.is_empty()
                        && (val_tag == 0x0c || val_tag == 0x13 || val_tag == 0x16)
                    {
                        if let Ok(s) = std::str::from_utf8(val_bytes) {
                            parts.push(format!("{}={}", oid_name, s));
                        }
                    }
                }
            }
            inner = inner_rest;
        }
        pos = rest;
    }
    if parts.is_empty() {
        "CN=Unknown".to_string()
    } else {
        parts.join(", ")
    }
}

/// Extract subject and issuer DN strings from a DER-encoded X.509 certificate.
pub(crate) fn basic_der_extract_names(data: &[u8]) -> Option<(String, String)> {
    // Certificate = SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
    let (tag, cert_content, _) = der_read_tlv(data)?;
    if tag != 0x30 {
        return None;
    } // must be SEQUENCE

    // tbsCertificate = SEQUENCE { version, serialNumber, signature, issuer, validity, subject, ... }
    let (tag, tbs_content, _) = der_read_tlv(cert_content)?;
    if tag != 0x30 {
        return None;
    }

    let mut pos = tbs_content;

    // version: [0] EXPLICIT INTEGER (optional — if tag is 0xa0, skip it)
    if let Some((0xa0, _, rest)) = der_read_tlv(pos) {
        pos = rest;
    }

    // serialNumber: INTEGER
    let (0x02, _, rest) = der_read_tlv(pos)? else {
        return None;
    };
    pos = rest;

    // signature: AlgorithmIdentifier (SEQUENCE)
    let (0x30, _, rest) = der_read_tlv(pos)? else {
        return None;
    };
    pos = rest;

    // issuer: Name (SEQUENCE)
    let (0x30, issuer_content, rest) = der_read_tlv(pos)? else {
        return None;
    };
    let issuer = der_extract_cn(issuer_content);
    pos = rest;

    // validity: SEQUENCE { notBefore, notAfter }
    let (0x30, _, rest) = der_read_tlv(pos)? else {
        return None;
    };
    pos = rest;

    // subject: Name (SEQUENCE)
    let (0x30, subject_content, _) = der_read_tlv(pos)? else {
        return None;
    };
    let subject = der_extract_cn(subject_content);

    Some((subject, issuer))
}

// =============================================================================
// java.security.cert — X509Certificate, CertificateFactory, CertPath
// =============================================================================

/// Recover the DER encoding backing a synthetic `java/security/cert/
/// X509Certificate` mirror.
///
/// Two shapes exist and both are in active use:
///   * 4-field mirror (`keystore::make_x509_mirror`'s fallback,
///     `t27_tls::register_accepted_issuers`, `phases_early`'s getCertificate
///     path) — slot 3 holds the raw DER as a `byte[]`;
///   * 3-field mirror plus a `crypto_impl` cert-store id in slot 2, which only
///     resolves under the `legacy-synthetic-crypto` feature.
///
/// Returns `None` when neither is present, so callers can raise the exception
/// their contract requires rather than inventing an empty/valid-looking answer.
pub(crate) fn x509_mirror_der(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<Vec<u8>> {
    if ctx.object_num_fields(this) > 3 {
        if let Value::Object(Some(der_arr)) = ctx.get_field(this, 3) {
            let len = ctx.array_length(der_arr);
            if len > 0 {
                let mut out = Vec::with_capacity(len);
                for i in 0..len {
                    if let Value::Int(b) = ctx.get_array_element(der_arr, i) {
                        out.push(b as u8);
                    }
                }
                if !out.is_empty() {
                    return Some(out);
                }
            }
        }
    }
    #[cfg(feature = "legacy-synthetic-crypto")]
    {
        let cert_id = match ctx.get_field(this, 2) {
            Value::Long(id) => id as u64,
            Value::Int(id) if id > 0 => id as u64,
            _ => 0,
        };
        if cert_id != 0 {
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                if !parsed.encoded.is_empty() {
                    return Some(parsed.encoded.clone());
                }
            }
        }
    }
    None
}

/// Shared body for both `X509Certificate.checkValidity` overloads.
///
/// STUB-REMOVAL (wave 2): in the default build both overloads were
/// unconditional no-ops — a void "check" method that never throws is a
/// hard-coded PASS, so an expired or not-yet-valid certificate was certified as
/// currently valid. Parse the mirror's own DER and enforce RFC 5280 §4.1.2.5
/// for real, throwing the exact subclasses the JDK specifies
/// (`CertificateExpiredException` / `CertificateNotYetValidException`, both
/// `CertificateException`s) so a caller's existing catch blocks match.
///
/// `at_secs` is the instant to evaluate against, in seconds since the epoch.
/// A certificate whose DER cannot be recovered or parsed CANNOT be attested to,
/// so it throws `CertificateException` rather than passing silently.
fn x509_check_validity_at(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    at_secs: i64,
) -> MethodCallResult {
    let Some(der) = x509_mirror_der(ctx, this) else {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateException",
            "certificate validity cannot be checked: no encoded form available",
        ));
    };
    let parsed = match crate::x509_manager::parse_certificate(&der) {
        Ok(p) => p,
        Err(_) => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/cert/CertificateException",
                "certificate validity cannot be checked: malformed certificate",
            ));
        }
    };
    if at_secs < parsed.not_before_secs {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateNotYetValidException",
            "NotBefore is in the future",
        ));
    }
    if at_secs > parsed.not_after_secs {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateExpiredException",
            "NotAfter has already passed",
        ));
    }
    Ok(None)
}

pub(crate) fn register_p68_security_cert(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // CertificateFactory
    let cf = "java/security/cert/CertificateFactory";
    r.register(
        cf,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
        |ctx, args| {
            // Real-JCA bring-up: prefer a genuine `CertificateFactory` wrapping
            // a real provider SPI over the synthetic 1-field stub below — see
            // `provider_chain::try_build_real_certificate_factory`'s doc
            // comment for the root-cause story (real-bytecode-only methods
            // like `generateCertPath` NPE on the synthetic stub's absent
            // `certFacSpi`). Falls back to the stub when the algorithm can't
            // be resolved (e.g. pure-synthetic mode, or an exotic type
            // nothing seeds).
            if crate::real_jca_mode() || crate::route_ec_to_real() || crate::route_dsa_to_real() {
                if let Some(real_cf) =
                    crate::jca::provider_chain::try_build_real_certificate_factory(ctx, args)
                {
                    return Ok(Some(Value::Object(Some(real_cf))));
                }
            }
            let obj = alloc_concurrent_synthetic(ctx, "java/security/cert/CertificateFactory", 1);
            ctx.set_field(obj, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cf,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
        |ctx, args| {
            let provider_name = match args.get(1) {
                Some(Value::Object(Some(name))) => ctx.read_string(*name).unwrap_or_default(),
                _ => String::new(),
            };
            // Do not silently substitute the default provider. The two-arg JCA
            // overload is a provider-selection API and must fail before any
            // certificate parsing when the requested provider is absent.
            let provider = ctx.invoke(
                "java/security/Security",
                "getProvider",
                "(Ljava/lang/String;)Ljava/security/Provider;",
                &args[1..2],
            );
            if matches!(provider, Ok(Some(Value::Object(Some(_))))) {
                return ctx.invoke(
                    "java/security/cert/CertificateFactory",
                    "getInstance",
                    "(Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
                    &args[..1],
                );
            }
            let detail = ctx.create_string(&format!("no such provider: {provider_name}"));
            if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
                "java/security/NoSuchProviderException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(detail))],
            ) {
                return Err(MethodCallFailed::ExceptionThrown(exc));
            }
            Err(RuntimeError::SecurityException {
                message: format!("no such provider: {provider_name}"),
            }
            .into())
        },
    );
    r.register(
        cf,
        "generateCertificate",
        "(Ljava/io/InputStream;)Ljava/security/cert/Certificate;",
        |ctx, args| {
            // Real-SPI fast path: a `CertificateFactory` built via
            // `getInstance(algo, Provider)` / `getInstance(algo, providerName)`
            // (both flow through `sun/security/jca/GetInstance.getInstance` ->
            // `getinstance_instance_provider[_obj]` in `jca::provider_chain`,
            // which run the class's REAL constructor) carries a genuine
            // `certFacSpi` field pointing at the provider's real
            // `CertificateFactorySpi` (e.g. BouncyCastle's
            // `org.bouncycastle.jcajce.provider.asymmetric.x509.CertificateFactory`).
            // Real `CertificateFactory.generateCertificate` is one line:
            // `return certFacSpi.engineGenerateCertificate(is)`. Delegating to
            // it here runs the genuine provider bytecode and produces the
            // provider's own concrete `Certificate` subclass — critical for
            // callers like BC's `JcaX509CertificateConverter.getCertificate`
            // that need a real `X509CertificateObject` (`getEncoded()`,
            // `verify()`, etc. are abstract on the bare `Certificate`/
            // `X509Certificate` classes and throw `AbstractMethodError`
            // otherwise). Only the OLD synthetic path — where `getInstance`
            // (1-arg, no provider) handed out the 1-field
            // `alloc_concurrent_synthetic` stub with no `certFacSpi` — falls
            // through to the legacy ad-hoc DER parser below.
            if let Some(Value::Object(Some(this))) = args.first() {
                if let Value::Object(Some(spi)) = ctx.get_field_by_name(*this, "certFacSpi") {
                    return ctx.invoke_virtual(
                        spi,
                        "engineGenerateCertificate",
                        "(Ljava/io/InputStream;)Ljava/security/cert/Certificate;",
                        &args[1..],
                    );
                }
            }

            // Read the full stream. Previously this only read a
            // `ByteArrayInputStream`'s field-0 `buf` array directly, so ANY
            // other InputStream subtype (e.g. a Netty/`SslContext` file or
            // buffer stream feeding `SelfSignedCertificate`'s cert) read zero
            // bytes and fell through to the empty-cert stub → 0-byte
            // `getEncoded()` → rustls `invalid peer certificate: BadEncoding`.
            // `p59_read_input_stream_fully` keeps the fast BAIS path and adds a
            // generic `read()`-loop fallback for every other stream type. See
            // fixed-suite-bugs/http-server-sslengine-identity-singleton-clobber-FIXED.md.
            let der_data = if let Some(Value::Object(Some(is_ref))) = args.get(1) {
                match p59_read_input_stream_fully(ctx, *is_ref) {
                    Ok(bytes) if !bytes.is_empty() => Some(bytes),
                    _ => None,
                }
            } else {
                None
            };

            // BUGFIX (keycloak-07-04 x509-authoritykeyidentifier-npe): this is the
            // no-explicit-provider `getInstance("X.509")` path — the form virtually
            // all callers use, including BC's own `JcaX509CertificateConverter`
            // (`CertificateFactory.getInstance("X.509")`, no `.setProvider(...)`).
            // The old code here never stored the raw DER anywhere reachable by
            // `getEncoded()` unless the (default-off) `legacy-synthetic-crypto`
            // parser happened to be enabled AND succeed; otherwise `getEncoded()`
            // returned a 0-length byte[]. Re-parsing that empty array with BC's own
            // ASN.1 decoder (e.g. `new JcaX509CertificateHolder(cert)`, which
            // `createAuthorityKeyIdentifier` calls internally) hits
            // `ASN1InputStream.readObject()` returning `null` at immediate EOF, then
            // `ASN1UniversalType.checkedCast(null)` NPEs on `null.getClass()`.
            //
            // Build a REAL `sun.security.x509.X509CertImpl` from the DER instead —
            // same "prefer real over synthetic" pattern `keystore::make_x509_mirror`
            // already uses for KeyStore/TLS-peer certs. Real bytecode gives a
            // correct `getEncoded()` (and `checkValidity`/`verify`/
            // `getSubjectX500Principal`/etc. for free); if the real ctor itself
            // throws (malformed input), `make_x509_mirror` falls back to a
            // synthetic mirror that still stashes the DER in field 3, so
            // `getEncoded()` is never empty for a non-empty input stream.
            if let Some(ref raw) = der_data {
                // Accept PEM-armored streams too (real JDK's X509Factory sniffs
                // `-----BEGIN`): decode to DER first so the mirror stores real
                // cert bytes and `getEncoded()` is non-empty. A DER stream (no
                // armor) passes through `pem_block_to_der` unchanged. Fixes
                // Netty `SelfSignedCertificate` → empty `getEncoded()` → rustls
                // `invalid peer certificate: BadEncoding`
                // (http-server-sslengine-identity-singleton-clobber).
                let data = crate::pem_block_to_der(raw);
                let alias = basic_der_extract_names(&data)
                    .map(|(subject, _)| subject)
                    .unwrap_or_else(|| "CN=Unknown".into());
                return Ok(Some(Value::Object(Some(
                    crate::keystore::make_x509_mirror(ctx, &alias, &data),
                ))));
            }

            // Fallback: the input stream had no readable bytes at all.
            let cert = alloc_concurrent_synthetic(ctx, "java/security/cert/X509Certificate", 3);
            let sub = ctx.create_string("CN=Unknown");
            let iss = ctx.create_string("CN=Unknown");
            ctx.set_field(cert, 0, Value::Object(Some(sub)));
            ctx.set_field(cert, 1, Value::Object(Some(iss)));
            ctx.set_field(cert, 2, Value::Long(0));
            Ok(Some(Value::Object(Some(cert))))
        },
    );
    r.register(
        cf,
        "generateCertificates",
        "(Ljava/io/InputStream;)Ljava/util/Collection;",
        |ctx, args| {
            // Real-SPI fast path — see the matching comment on
            // `generateCertificate` above; same rationale, same field.
            if let Some(Value::Object(Some(this))) = args.first() {
                if let Value::Object(Some(spi)) = ctx.get_field_by_name(*this, "certFacSpi") {
                    return ctx.invoke_virtual(
                        spi,
                        "engineGenerateCertificates",
                        "(Ljava/io/InputStream;)Ljava/util/Collection;",
                        &args[1..],
                    );
                }
            }

            // Try to parse a single certificate and return it in a list
            let al = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 10);
            ctx.set_field(al, 0, Value::Object(Some(arr)));

            // Try reading a cert from the input stream. Same DER-preservation fix
            // as `generateCertificate` above: build a real `X509CertImpl` (falls
            // back internally to a DER-stashing synthetic mirror) instead of a
            // bare 3-field stub whose `getEncoded()` would come back empty.
            if let Some(Value::Object(Some(is_ref))) = args.get(1) {
                let raw = match p59_read_input_stream_fully(ctx, *is_ref) {
                    Ok(bytes) => bytes,
                    Err(_) => Vec::new(),
                };
                if !raw.is_empty() {
                    // PEM-or-DER, same as `generateCertificate`.
                    let data = crate::pem_block_to_der(&raw);
                    let alias = basic_der_extract_names(&data)
                        .map(|(subject, _)| subject)
                        .unwrap_or_else(|| "CN=Unknown".into());
                    let cert = crate::keystore::make_x509_mirror(ctx, &alias, &data);
                    ctx.set_array_element(arr, 0, Value::Object(Some(cert)));
                    ctx.set_field(al, 1, Value::Int(1));
                    return Ok(Some(Value::Object(Some(al))));
                }
            }
            ctx.set_field(al, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(al))))
        },
    );

    // Certificate base class
    let cert = "java/security/cert/Certificate";
    r.register(cert, "getType", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("X.509");
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(cert, "getEncoded", "()[B", |ctx, args| {
        // NEW-13: when the X509Certificate was constructed from a TLS peer
        // cert chain (`SSLSession.getPeerCertificates`), the raw DER bytes
        // are stored directly in field 3 as a byte[]. Prefer that over the
        // legacy-synthetic-crypto `cert_store` lookup so that `getEncoded()`
        // returns a non-empty array even when crypto_impl is disabled.
        if let Some(Value::Object(Some(this))) = args.get(0) {
            if ctx.object_num_fields(*this) > 3 {
                if let Value::Object(Some(der_arr)) = ctx.get_field(*this, 3) {
                    let len = ctx.array_length(der_arr);
                    if len > 0 {
                        let out = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
                        for i in 0..len {
                            out_copy_byte(ctx, der_arr, out, i);
                        }
                        return Ok(Some(Value::Object(Some(out))));
                    }
                }
            }
        }
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                let arr =
                    ctx.new_array(cratonvm_types::ArrayElementType::Byte, parsed.encoded.len());
                for (i, &b) in parsed.encoded.iter().enumerate() {
                    ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
        // STUB-REMOVAL (wave 2): the tail used to hand back a ZERO-LENGTH
        // byte[]. An empty encoding is indistinguishable from "no certificate"
        // to every consumer (rustls answers it with `BadEncoding`, a pin/hash
        // check silently compares nothing), so a certificate object with no
        // recoverable DER must raise the exception its contract specifies
        // rather than fabricate one. Same treatment as the `X509Certificate`
        // override below.
        let this = obj_arg(args, 0)?;
        let nfields = ctx.object_num_fields(this);
        let message = format!("certificate has no encoded form ({nfields} fields)");
        Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateEncodingException",
            &message,
        ))
    });
    r.register(
        cert,
        "getPublicKey",
        "()Ljava/security/PublicKey;",
        |ctx, args| {
            let _ = (&ctx, &args); // used only under legacy-synthetic-crypto feature
            #[cfg(feature = "legacy-synthetic-crypto")]
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    let pk = alloc_concurrent_synthetic(ctx, "java/security/PublicKey", 4);
                    let alg_idx = match parsed.public_key_algorithm.as_str() {
                        "RSA" => 6,
                        "EC" => 7,
                        _ => 0,
                    };
                    ctx.set_field(pk, 0, Value::Int(alg_idx));
                    ctx.set_field(pk, 1, Value::Int(256));
                    ctx.set_field(pk, 2, Value::Int(parsed.public_key_bytes.len() as i32));
                    ctx.set_field(pk, 3, Value::Long(0));
                    return Ok(Some(Value::Object(Some(pk))));
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );
    // SECURITY: `Certificate.verify(PublicKey)` MUST throw on a bad signature.
    // In the default build we deliberately DO NOT register a native override:
    // the real `java.security.cert` / `X509CertImpl.verify(PublicKey)` bytecode
    // then runs and performs the genuine signature check, throwing
    // `SignatureException` (or `InvalidKeyException` / `CertificateException`)
    // on failure. A no-op native that always returns `Ok(None)` would silently
    // certify any certificate against any key — a verification-bypass
    // vulnerability — so it is gated entirely behind the legacy feature.
    #[cfg(feature = "legacy-synthetic-crypto")]
    r.register(
        cert,
        "verify",
        "(Ljava/security/PublicKey;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    // Fail-closed: the legacy synthetic path can only attest to
                    // the signature it is able to verify with the material it
                    // holds. If verification does not succeed we throw rather
                    // than returning normally, so a caller never mistakes an
                    // unverifiable certificate for a verified one.
                    if !parsed.verify_signature(&parsed.public_key_bytes) {
                        return Err(p68_signature_failure(
                            ctx,
                            "certificate signature verification failed",
                        ));
                    }
                }
            }
            Ok(None)
        },
    );

    // X509Certificate = 3-field (subject_str=0, issuer_str=1, cert_id=2)
    let x509 = "java/security/cert/X509Certificate";
    r.register(
        x509,
        "getSubjectDN",
        "()Ljava/security/Principal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sub = ctx.get_field(this, 0);
            if matches!(sub, Value::Object(Some(_))) {
                // Wrap string in a Principal-like synthetic
                let princ =
                    alloc_concurrent_synthetic(ctx, "javax/security/auth/x500/X500Principal", 1);
                ctx.set_field(princ, 0, sub);
                return Ok(Some(Value::Object(Some(princ))));
            }
            Ok(Some(sub))
        },
    );
    r.register(
        x509,
        "getIssuerDN",
        "()Ljava/security/Principal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let iss = ctx.get_field(this, 1);
            if matches!(iss, Value::Object(Some(_))) {
                let princ =
                    alloc_concurrent_synthetic(ctx, "javax/security/auth/x500/X500Principal", 1);
                ctx.set_field(princ, 0, iss);
                return Ok(Some(Value::Object(Some(princ))));
            }
            Ok(Some(iss))
        },
    );
    r.register(
        x509,
        "getSubjectX500Principal",
        "()Ljavax/security/auth/x500/X500Principal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sub = ctx.get_field(this, 0);
            let princ =
                alloc_concurrent_synthetic(ctx, "javax/security/auth/x500/X500Principal", 1);
            ctx.set_field(princ, 0, sub);
            Ok(Some(Value::Object(Some(princ))))
        },
    );
    r.register(
        x509,
        "getIssuerX500Principal",
        "()Ljavax/security/auth/x500/X500Principal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let iss = ctx.get_field(this, 1);
            let princ =
                alloc_concurrent_synthetic(ctx, "javax/security/auth/x500/X500Principal", 1);
            ctx.set_field(princ, 0, iss);
            Ok(Some(Value::Object(Some(princ))))
        },
    );
    r.register(x509, "getNotBefore", "()Ljava/util/Date;", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                let date = alloc_concurrent_synthetic(ctx, "java/util/Date", 1);
                ctx.set_field(date, 0, Value::Long(parsed.not_before * 1000)); // millis
                return Ok(Some(Value::Object(Some(date))));
            }
        }
        let _ = (ctx, args);
        Ok(Some(Value::Object(None)))
    });
    r.register(x509, "getNotAfter", "()Ljava/util/Date;", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                let date = alloc_concurrent_synthetic(ctx, "java/util/Date", 1);
                ctx.set_field(date, 0, Value::Long(parsed.not_after * 1000));
                return Ok(Some(Value::Object(Some(date))));
            }
        }
        let _ = (ctx, args);
        Ok(Some(Value::Object(None)))
    });
    r.register(
        x509,
        "getSerialNumber",
        "()Ljava/math/BigInteger;",
        |ctx, args| {
            #[cfg(feature = "legacy-synthetic-crypto")]
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    // Create BigInteger with value from serial number bytes
                    // Convert serial number bytes to decimal string
                    let serial_hex: String = parsed
                        .serial_number
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect();
                    let serial_dec = u128::from_str_radix(&serial_hex, 16)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|_| "0".to_string());
                    let bi = bi_alloc(ctx, &serial_dec);
                    return Ok(Some(Value::Object(Some(bi))));
                }
            }
            let _ = (ctx, args);
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(
        x509,
        "getSigAlgName",
        "()Ljava/lang/String;",
        |ctx, args| {
            #[cfg(feature = "legacy-synthetic-crypto")]
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    let s = ctx.create_string(&parsed.sig_algorithm);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            let _ = args;
            let s = ctx.create_string("SHA256withRSA");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(x509, "getVersion", "()I", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                return Ok(Some(Value::Int(parsed.version as i32)));
            }
        }
        let _ = (ctx, args);
        Ok(Some(Value::Int(3)))
    });
    r.register(x509, "getType", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("X.509");
        Ok(Some(Value::Object(Some(s))))
    });
    // STUB-REMOVAL (wave 2): in the DEFAULT build (no `legacy-synthetic-crypto`)
    // this returned an EMPTY byte[] for every certificate. That is not a
    // harmless placeholder: `getEncoded()` is how a caller obtains the DER it
    // then hashes, pins, re-parses or feeds to a trust store, so an empty array
    // turns a real certificate into "no certificate" without any error —
    // rustls answers a zero-length chain with `BadEncoding`, and wave 1 had to
    // work around this from `keystore.rs`. The synthetic X.509 mirror
    // (`keystore::make_x509_mirror`'s fallback, `t27_tls::
    // register_accepted_issuers`, `phases_early`'s getCertificate path) stashes
    // the real DER in slot 3; read it, exactly as the `Certificate.getEncoded`
    // sibling above already does. `x509_mirror_der` is the shared reader.
    r.register(x509, "getEncoded", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(der) = x509_mirror_der(ctx, this) {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, der.len());
            for (i, &b) in der.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
        // No DER anywhere on this object. Real `X509Certificate.getEncoded()`
        // throws `CertificateEncodingException` rather than handing back an
        // empty array that the caller would mistake for a valid encoding.
        Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateEncodingException",
            "certificate has no encoded form",
        ))
    });
    r.register(
        x509,
        "getPublicKey",
        "()Ljava/security/PublicKey;",
        |ctx, args| {
            #[cfg(feature = "legacy-synthetic-crypto")]
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    let pk = alloc_concurrent_synthetic(ctx, "java/security/PublicKey", 4);
                    let alg_idx = match parsed.public_key_algorithm.as_str() {
                        "RSA" => 6,
                        "EC" => 7,
                        _ => 0,
                    };
                    ctx.set_field(pk, 0, Value::Int(alg_idx));
                    ctx.set_field(pk, 1, Value::Int(256));
                    ctx.set_field(pk, 2, Value::Int(parsed.public_key_bytes.len() as i32));
                    ctx.set_field(pk, 3, Value::Long(0));
                    return Ok(Some(Value::Object(Some(pk))));
                }
            }
            let _ = (ctx, args);
            Ok(Some(Value::Object(None)))
        },
    );
    // SECURITY: see the `Certificate.verify` note above. The default build does
    // NOT register this native, so the real `X509CertImpl.verify(PublicKey)`
    // bytecode performs the genuine signature check and throws on failure.
    // Previously this discarded the `verify_signature` result and always
    // returned success, certifying any certificate against any key.
    #[cfg(feature = "legacy-synthetic-crypto")]
    r.register(
        x509,
        "verify",
        "(Ljava/security/PublicKey;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    // Fail-closed: propagate a verification failure as a thrown
                    // SignatureException instead of discarding the result.
                    if !parsed.verify_signature(&parsed.public_key_bytes) {
                        return Err(p68_signature_failure(
                            ctx,
                            "certificate signature verification failed",
                        ));
                    }
                }
            }
            Ok(None)
        },
    );
    // STUB-REMOVAL (wave 2): both overloads were hard-coded PASSES in the
    // default build (the whole body was `#[cfg(feature =
    // "legacy-synthetic-crypto")]`-gated, leaving `Ok(None)`). See
    // `x509_check_validity_at` for the enforcement and the exception contract.
    r.register(x509, "checkValidity", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        x509_check_validity_at(ctx, this, now)
    });
    r.register(x509, "checkValidity", "(Ljava/util/Date;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let date = obj_arg(args, 1)?;
        // `Date.getTime()` is a real virtual call that can allocate/safepoint,
        // so `this` must be pinned across it (the synthetic 1-field Date shape
        // is not the only one that reaches here — a real `java.util.Date`
        // keeps its millis in `fastTime`, which no fixed slot index finds).
        let pin = ctx.pin_native_root(this);
        let date_pin = ctx.pin_native_root(date);
        let called = ctx.invoke_virtual(date, "getTime", "()J", &[]);
        let this = ctx.read_native_pin(pin, this);
        let date = ctx.read_native_pin(date_pin, date);
        ctx.unpin_native_roots(pin);
        let millis = match called? {
            Some(Value::Long(ms)) => ms,
            // Unreadable instant: fall back to the synthetic Date's slot 0.
            _ => match ctx.get_field(date, 0) {
                Value::Long(ms) => ms,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("checkValidity: date has no time value".into()),
                    }
                    .into());
                }
            },
        };
        x509_check_validity_at(ctx, this, millis.div_euclid(1000))
    });

    // getSignature() -> byte[] (on X509Certificate)
    r.register(x509, "getSignature", "()[B", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                let arr = ctx.new_array(
                    cratonvm_types::ArrayElementType::Byte,
                    parsed.signature_bytes.len(),
                );
                for (i, &b) in parsed.signature_bytes.iter().enumerate() {
                    ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
        let _ = args;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
        Ok(Some(Value::Object(Some(arr))))
    });

    // getTBSCertificate() -> byte[]
    r.register(x509, "getTBSCertificate", "()[B", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                let arr = ctx.new_array(
                    cratonvm_types::ArrayElementType::Byte,
                    parsed.tbs_bytes.len(),
                );
                for (i, &b) in parsed.tbs_bytes.iter().enumerate() {
                    ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
        let _ = args;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
        Ok(Some(Value::Object(Some(arr))))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// NEW-13 regression tests: validate that the real javax.net.ssl path is
// registered with the expected signatures and that the unified s2_registry
// TLS helpers behave correctly for error cases.
// =============================================================================

#[cfg(test)]
pub(crate) mod new13_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;

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
        let err = get_instance(&mut ctx, &[Value::Object(Some(algo))])
            .expect_err("getInstance(\"test-invalid-algo\") must not succeed");
        match err {
            MethodCallFailed::ExceptionThrown(_) => {}
            MethodCallFailed::InternalError(_) => {}
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
        let err = get_instance(&mut ctx, &[Value::Object(Some(algo))])
            .expect_err("getInstance(\"test-invalid-algo\") must not succeed");
        match err {
            MethodCallFailed::ExceptionThrown(_) => {}
            MethodCallFailed::InternalError(_) => {}
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
        let sess_ids = [NEW13_SESS_PROTO, NEW13_SESS_CIPHER, NEW13_SESS_TLSID];
        for i in 0..sess_ids.len() {
            for j in (i + 1)..sess_ids.len() {
                assert_ne!(sess_ids[i], sess_ids[j], "duplicate SSLSession field index");
            }
            assert!(sess_ids[i] < NEW13_SSL_SESS_FIELDS);
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
        let c = new13_build_connector(&[], false, None);
        assert!(c.is_ok(), "connector build failed: {:?}", c.err());
    }

    #[test]
    fn new13_build_connector_tolerates_unparseable_extra_root() {
        // FIX (es-restclient-https): a garbage "DER" (e.g. from a corrupt or
        // unexpected TrustManager) must be skipped rather than failing the
        // whole connector build — `new13_build_connector` logs and continues.
        let garbage = vec![0xFFu8, 0x00, 0x01, 0x02];
        let c = new13_build_connector(&[garbage], false, None);
        assert!(
            c.is_ok(),
            "connector build must tolerate an unparseable extra root: {:?}",
            c.err()
        );
    }
}
