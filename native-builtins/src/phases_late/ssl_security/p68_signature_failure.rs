#[allow(unused_imports)]
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
pub fn p68_signature_failure(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/security/SignatureException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
        || {
            RuntimeError::SecurityException {
                message: msg.to_string(),
            }
            .into()
        },
    )
}

// =============================================================================
// javax.crypto.Mac — real HMAC over the six algorithms `mac_compute_hmac`
// implements (HmacMD5 / HmacSHA1 / HmacSHA224 / HmacSHA256 / HmacSHA384 /
// HmacSHA512), and a NoSuchAlgorithmException for every other name. The header used to read
// "Real HMAC support using SHA-256", which was accurate in a way nobody meant:
// SHA-256 was the fallback for every algorithm the engine did not implement,
// so a caller asking for HmacSHA3-256 got HMAC-SHA-256 bytes with no error.
// See `mac_algorithm_supported`.
// 4-field synthetic:
//   0 = algorithm (String)
//   1 = key object (Key — has getEncoded() returning byte[])
//   2 = initialized (Int: 0=no, 1=yes)
//   3 = accumulated data (byte[] array)
// =============================================================================

/// Helper: read raw bytes from a JVM byte array object
pub fn mac_read_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            bytes.push(b as u8);
        }
    }
    bytes
}

/// A genuine, CHECKED `javax.crypto.ShortBufferException` for the output-buffer
/// overload of `doFinal`.
///
/// `ShortBufferException extends GeneralSecurityException`, so it is checked and
/// a caller's `catch (ShortBufferException)` — or `catch
/// (GeneralSecurityException)` — only matches a real instance. A
/// `RuntimeError::IllegalArgumentException` here would be unchecked and would
/// sail straight past that handler, which is the mistake `md_get_instance` and
/// `mac_no_such_algorithm` both record having made once already.
pub fn mac_short_buffer(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "javax/crypto/ShortBufferException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IllegalArgumentException {
        message: msg.to_string(),
    }
    .into()
}

/// A `java.security.Provider` object naming `SunJCE` — the provider these HMACs
/// are advertised under by
/// `jca::provider_chain::seed_retired_getalgorithms_literals`, so
/// `Mac.getProvider().getName()` agrees with `Security.getProviders()` instead
/// of contradicting it. Mirrors `jca::message_digest::md_get_provider`, which
/// does the same for `SUN`.
pub fn jce_provider_object(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    crate::jca::make_named_provider(ctx, "SunJCE")
}

/// Helper: extract the key bytes from a Key object (field 0 = encoded byte[])
pub fn mac_extract_key_bytes(ctx: &mut dyn NativeContext, key_obj: ObjectRef) -> Vec<u8> {
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
///
/// gc-common w15-d: keyed [`mac_state_key`] -- `(VM, identity hash)` -- not
/// the bare hash. Identity hashes are a per-heap counter, so two VMs in one
/// process mint the same hashes in the same order: VM B's `Mac` read, re-keyed
/// and reset VM A's state -- key material included. A row now also goes with
/// its `Mac` and with its VM ([`forget_vm_factory_identity_rows`]); the size
/// cap below stays as a backstop.
///
/// gc-common w27-a: keyed [`SslObjKey`] -- `(VM, the Mac's weak lock key)` --
/// not `(VM, identity hash)`: two LIVE `Mac`s of one VM that drew one hash
/// shared one row, so one's `init` re-keyed the other and each `doFinal` MACed
/// the other's accumulated bytes under the other's key. The row goes when the
/// lock-key sweep frees the key ([`forget_ssl_security_obj_keys`]).
#[derive(Default, Clone)]
pub struct MacState {
    pub algo: String,
    pub key: Vec<u8>,
    pub data: Vec<u8>,
    pub initialized: bool,
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
pub fn mac_zeroize(buf: &mut Vec<u8>) {
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
pub const MAC_STATE_MAX_ENTRIES: usize = 4096;

#[allow(clippy::type_complexity)]
pub fn mac_state_table(
) -> &'static std::sync::Mutex<std::collections::HashMap<SslObjKey, MacState>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<SslObjKey, MacState>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The [`mac_state_table`] key of `mac` ([`ssl_obj_key`]; gc-common w27-a).
/// Every `Mac` native writes or expects a row, so it mints.
pub fn mac_state_key(ctx: &dyn NativeContext, mac: ObjectRef) -> SslObjKey {
    ssl_obj_key(ctx, mac)
}

/// The key of every per-object row this module keeps -- the `Mac` states, the
/// `SSLSocket` rows, the `KeyManagerFactory` / `TrustManagerFactory` rows and
/// the staged `createSocket()` trust scopes: `(calling VM, the object's weak
/// lock key)` (gc-common w27-a,
/// `common-w26b-tls-sibling-tables-keyed-by-vm-folded-identity-hash`).
///
/// It was `(VM, identity hash)`. The hash is 32 bits from one per-heap counter
/// that wraps, so two LIVE objects of one VM can share it, and then they
/// shared every row: one `SSLSocket`'s staged trust scope was consumed by
/// another's `connect()`, one factory's key-manager id served to another. A
/// weak lock key (`lib.rs::gc_stable_weak_lock_key`) is matched by the
/// object's current address, re-addressed after every moving collection, and
/// never minted again once its object died; the lock-key sweep hands the
/// freed keys to [`forget_ssl_security_obj_keys`], which drops their rows.
///
/// The VM half is redundant with the key (a key belongs to one VM), but it
/// lets VM teardown and the per-VM scans (`factory_rows_name_manager_id`,
/// `keystore`'s store-release check) filter rows without asking the lock-key
/// registry per row.
pub type SslObjKey = (usize, u64);

/// [`SslObjKey`] of `obj`, minting its weak lock key if it has none. `obj`
/// must be its CURRENT address (a native's argument, or a pin / handle re-read
/// before anything that can allocate): a stale address mints a key of its own
/// at the vacated address, whose rows nothing reads. Computed BEFORE any
/// table guard is taken -- it takes the lock-key registry's mutex.
pub fn ssl_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> SslObjKey {
    let key = crate::gc_stable_weak_lock_key(ctx, obj)
        .map(|k| k as u64)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"));
    (ctx.vm_identity(), key)
}

/// The [`SslObjKey`] [`ssl_obj_key`] would answer for `obj`, or `None` when the
/// calling VM never keyed `obj` -- in which case no table of this module has a
/// row for it. Never mints: for the read and remove paths.
pub fn existing_ssl_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<SslObjKey> {
    crate::existing_weak_lock_key(ctx, obj).map(|k| (ctx.vm_identity(), k as u64))
}

/// The rows of `map` filed under any of `keys`, removed and handed back (so a
/// caller can finish with them outside the guard). Walks whichever side is
/// smaller: a sweep frees many keys, and most tables here are small.
pub fn take_ssl_rows<V, S: std::hash::BuildHasher>(
    map: &mut std::collections::HashMap<SslObjKey, V, S>,
    keys: &rustc_hash::FxHashSet<SslObjKey>,
) -> Vec<V> {
    if map.is_empty() {
        return Vec::new();
    }
    if map.len() <= keys.len() {
        let hit: Vec<SslObjKey> = map.keys().filter(|k| keys.contains(*k)).copied().collect();
        hit.into_iter().filter_map(|k| map.remove(&k)).collect()
    } else {
        keys.iter().filter_map(|k| map.remove(k)).collect()
    }
}

/// [`take_ssl_rows`] for a set; answers how many marks went.
pub fn take_ssl_marks<S: std::hash::BuildHasher>(
    set: &mut std::collections::HashSet<SslObjKey, S>,
    keys: &rustc_hash::FxHashSet<SslObjKey>,
) -> usize {
    if set.is_empty() {
        return 0;
    }
    let before = set.len();
    if set.len() <= keys.len() {
        set.retain(|k| !keys.contains(k));
    } else {
        for k in keys {
            set.remove(k);
        }
    }
    before - set.len()
}

/// The weak lock keys `keys` of VM `vm` were freed -- their objects died, or
/// `vm` is being torn down (`lib.rs::sweep_lock_keys` /
/// `forget_vm_lock_keys`): drop every row this module filed under them, and
/// answer how many went (gc-common w27-a).
///
/// * `Mac` states (scrubbed on drop, outside the lock);
/// * the four factory tables and the initialised marks; the key-manager and
///   trust-manager ids a dropped factory row named are OFFERED for release
///   (`t27_tls::offer_manager_state`) and freed by the VM's next
///   `t27_tls::gc_sweep_tls_rows` unless a live carrier still names them --
///   one collection later than the old `(vm, hash)` weak-owner route, which
///   ran inside that sweep; never earlier;
/// * the `SSLSocket` rows: client-auth, endpoint algorithm, the staged
///   `createSocket()` trust scope, the parked `connect()` connection (its
///   `TcpStream` closed outside the lock), the pending layered handshake
///   (`t27_tls::forget_pending_layered_sockets_of`), and the
///   `HandshakeCompletedListener` rows, whose global roots are queued for the
///   VM's next socket native ([`release_dead_socket_listener_roots`]) because
///   a sweep has no `ctx`;
/// * the `p68` custom trust anchors of a dead `SSLSocketFactory`. An
///   `SSLContext`'s own row is keyed by the context's lock key, which
///   `t27_tls` holds (`gc_stable_lock_key`), so it never reaches this hook: its
///   `P68TrustRoots` weak-owner row drops it ([`forget_p68_trust_roots`]).
///
/// One lock at a time, none nested; nothing reaches the VM. Every table is
/// `LockLevel::Scratch` or a raw leaf.
pub fn forget_ssl_security_obj_keys(vm: usize, keys: &[usize]) -> usize {
    if keys.is_empty() {
        return 0;
    }
    let keys: rustc_hash::FxHashSet<SslObjKey> = keys.iter().map(|k| (vm, *k as u64)).collect();
    let mut dropped = 0usize;
    let macs: Vec<MacState> = {
        let mut t = mac_state_table().lock().unwrap_or_else(|e| e.into_inner());
        take_ssl_rows(&mut *t, &keys)
    };
    dropped += macs.len();
    drop(macs); // scrubs the key bytes, outside the lock
                // Factory rows. Each guard is its own statement (lesson dd).
    let ks_rows = take_ssl_rows(&mut *kmf_keystore_id_by_identity().lock(), &keys);
    dropped += ks_rows.len();
    let cached = take_ssl_rows(&mut *kmf_key_manager_cache().lock(), &keys);
    dropped += cached.len();
    let live_km = take_ssl_rows(&mut *kmf_live_km_id_by_identity().lock(), &keys);
    dropped += live_km.len();
    let tm_rows = take_ssl_rows(&mut *tmf_tm_id_by_identity().lock(), &keys);
    dropped += tm_rows.len();
    for km in cached.into_iter().map(|(_, km)| km).chain(live_km) {
        offer_replaced_manager_id(vm, true, Some(km), None);
    }
    for tm in tm_rows {
        offer_replaced_manager_id(vm, false, Some(tm), None);
    }
    if !ks_rows.is_empty() {
        // A store a dead factory's row was the last holder of goes in this
        // collection, as it did when the TLS sweep dropped the row (the
        // keystore registry and the factory table are locked one at a time).
        dropped += crate::keystore::release_store_candidates(vm);
    }
    dropped += take_ssl_marks(&mut *jsse_factory_initialized().lock(), &keys);
    dropped += take_ssl_rows(&mut *p68_ctx_trust_roots_table().lock(), &keys).len();
    // `SSLSocket` rows.
    dropped += take_ssl_rows(&mut *ssl_sock_auth_state().lock(), &keys).len();
    dropped += take_ssl_rows(&mut *ssl_sock_endpoint_alg().lock(), &keys).len();
    dropped += take_ssl_rows(&mut *pending_ssl_socket_connect_ctx_table().lock(), &keys).len();
    let parked: Vec<PendingConnectSocket> = {
        let mut pending = pending_connect_sockets().lock();
        if pending.is_empty() {
            Vec::new()
        } else {
            let ids: Vec<i32> = pending
                .iter()
                .filter(|(_, p)| keys.contains(&(p.vm, p.sock_key)))
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect()
        }
    };
    dropped += parked.len();
    drop(parked); // closes each parked `TcpStream`, outside the lock
    let layered = take_ssl_marks_listed(&mut *ssl_layered_socket_owners().lock(), &keys);
    for (owner_vm, key) in layered {
        dropped += crate::t27_tls::forget_pending_layered_sockets_of(owner_vm, key);
    }
    let listener_rows = take_ssl_rows(&mut *handshake_listeners().lock(), &keys);
    if !listener_rows.is_empty() {
        dropped += listener_rows.len();
        let handles: Vec<usize> = listener_rows
            .into_iter()
            .flatten()
            .map(|(_, handle)| handle)
            .collect();
        if !handles.is_empty() {
            dead_socket_listener_roots()
                .lock()
                .entry(vm)
                .or_default()
                .extend(handles);
        }
    }
    dropped
}

/// [`take_ssl_marks`] that hands the removed marks back.
pub fn take_ssl_marks_listed<S: std::hash::BuildHasher>(
    set: &mut std::collections::HashSet<SslObjKey, S>,
    keys: &rustc_hash::FxHashSet<SslObjKey>,
) -> Vec<SslObjKey> {
    if set.is_empty() {
        return Vec::new();
    }
    let hit: Vec<SslObjKey> = if set.len() <= keys.len() {
        set.iter().filter(|k| keys.contains(*k)).copied().collect()
    } else {
        keys.iter().filter(|k| set.contains(*k)).copied().collect()
    };
    for k in &hit {
        set.remove(k);
    }
    hit
}

/// Bound the Mac state table: if it has grown past the cap, drop excess entries
/// (scrubbing their key material via `MacState::drop`). Called from `getInstance`
/// just before inserting a fresh entry. `keep` is the id we are about to insert,
/// which is never evicted. Best-effort eviction (lowest ids first) — these are
/// abandoned Mac handles whose Java objects are unreachable.
///
/// Generic over the key since gc-common w15-d (the table is keyed
/// [`mac_state_key`] now; the `phases_late` unit test still drives it with
/// `i32` ids). The natives call [`mac_state_evict_for_vm_if_needed`].
#[cfg_attr(not(test), allow(dead_code))]
pub fn mac_state_evict_if_needed<K>(t: &mut std::collections::HashMap<K, MacState>, keep: K)
where
    K: Copy + Ord + Eq + std::hash::Hash,
{
    mac_state_evict_matching(t, keep, |_| true);
}

/// [`mac_state_evict_if_needed`] over the calling VM's rows only: `keep` is a
/// [`mac_state_key`], and the cap counts, and evicts from, the rows that share
/// its VM half (gc-common w16-d).
///
/// The table is process-wide and the eviction took the LOWEST keys first.
/// Since w15-d folds the VM into the upper half of every key, that meant VM
/// B's `getInstance` at the cap evicted the lowest-numbered VM's rows --
/// possibly all of them, and every one possibly a live `Mac`, whose next
/// `doFinal` then answered `MAC not initialized` or lost its algorithm. One
/// VM's `Mac` churn no longer reaches another VM's key material; dead `Mac`s
/// of every VM leave with the collection that finds them dead
/// ([`forget_ssl_security_obj_keys`] since gc-common w27-a), so the cap stays
/// only the per-VM backstop it was meant to be.
///
/// gc-common w27-a: the key is an [`SslObjKey`] now, so the VM is its first
/// half rather than the upper 32 bits.
pub fn mac_state_evict_for_vm_if_needed(
    t: &mut std::collections::HashMap<SslObjKey, MacState>,
    keep: SslObjKey,
) {
    let vm = keep.0;
    mac_state_evict_matching(t, keep, |k| k.0 == vm);
}

/// Shared body: if at least [`MAC_STATE_MAX_ENTRIES`] rows satisfy `mine`,
/// drop the lowest-keyed of them (never `keep`) down to half the cap.
pub fn mac_state_evict_matching<K>(
    t: &mut std::collections::HashMap<K, MacState>,
    keep: K,
    mine: impl Fn(&K) -> bool,
) where
    K: Copy + Ord + Eq + std::hash::Hash,
{
    if t.len() < MAC_STATE_MAX_ENTRIES {
        return;
    }
    let mut ids: Vec<K> = t.keys().copied().filter(|k| mine(k)).collect();
    if ids.len() < MAC_STATE_MAX_ENTRIES {
        return;
    }
    let target = MAC_STATE_MAX_ENTRIES / 2;
    let to_remove = ids.len().saturating_sub(target);
    ids.retain(|&k| k != keep);
    ids.sort_unstable();
    for id in ids.into_iter().take(to_remove) {
        // Removing drops the MacState, whose Drop zeroes the key bytes.
        t.remove(&id);
    }
}

/// The application `MacSpi` a `javax.crypto.Mac` wraps, if it wraps one.
///
/// Every `Mac` built by `mac_get_instance`'s own path keeps its state in
/// `mac_state_table` and leaves the real `spi` field null; one built through
/// the JDK's own `(MacSpi, Provider, String)` constructor — which is what
/// `provider_chain::build_real_mac` does for a third-party provider — always
/// has it. So the field IS the discriminator, the same one
/// `skf_receiver_is_ours` uses for `SecretKeyFactory` and `kf_delegate_spi`
/// for `KeyFactory`.
///
/// Every native registered on `javax/crypto/Mac` consults this first. Without
/// it they shadowed the real bytecode for a receiver they did not build, so a
/// `Mac` obtained from BouncyCastle computed an HMAC of this VM's choosing —
/// and for the BC-only MACs (`CMAC`, `Poly1305`, `GOST28147MAC`, the
/// `*-CMAC`/`*-GMAC` families) `getInstance` refused a name the provider
/// implements.
pub fn mac_delegate_spi(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "spi") {
        Value::Object(Some(spi)) => Some(spi),
        _ => None,
    }
}

/// `doFinal()` / `doFinal(byte[])` on a delegated `Mac`: `engineUpdate(input)`
/// when there is input, then `engineDoFinal`, then `engineReset`, answering
/// the array `engineDoFinal` produced.
///
/// gc-common w15-d: every one of the three is Java, so `spi` and the result
/// array are rooted across the calls that follow them. The inline copies this
/// replaces pinned `spi` only AFTER `engineUpdate` (the pin then rooted the
/// pre-move address), returned the `engineDoFinal` array through its address
/// from before `engineReset`, and leaked the pin when `engineDoFinal` threw.
pub fn mac_delegate_do_final(
    ctx: &mut dyn NativeContext,
    spi: ObjectRef,
    input: Option<ObjectRef>,
) -> MethodCallResult {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let spi_h = scope.root(spi);
    if let Some(input) = input {
        let len = scope.array_length(input) as i32;
        let spi_now = scope.get(&spi_h);
        scope.invoke_virtual(
            spi_now,
            "engineUpdate",
            "([BII)V",
            &[Value::Object(Some(input)), Value::Int(0), Value::Int(len)],
        )?;
    }
    let spi_now = scope.get(&spi_h);
    let produced = scope.invoke_virtual(spi_now, "engineDoFinal", "()[B", &[])?;
    let produced_h = match produced {
        Some(Value::Object(Some(o))) => Some(scope.root(o)),
        _ => None,
    };
    let spi_now = scope.get(&spi_h);
    scope.invoke_virtual(spi_now, "engineReset", "()V", &[])?;
    Ok(match produced_h {
        Some(h) => Some(Value::Object(Some(scope.get(&h)))),
        None => produced,
    })
}

/// `doFinal(byte[] out, int outOffset)` on a delegated `Mac` (`args` is the
/// native's own argument slice).
///
/// gc-common w15-d: `engineReset` used to be invoked on the `spi` address read
/// before `engineDoFinal` ran; both `spi` and `out` are rooted now.
pub fn mac_delegate_do_final_into(
    ctx: &mut dyn NativeContext,
    spi: ObjectRef,
    args: &[Value],
) -> MethodCallResult {
    let out_arg = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Cannot store MAC in output buffer".to_string()),
            }
            .into())
        }
    };
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let spi_h = scope.root(spi);
    let out_h = scope.root(out_arg);
    let produced = scope.invoke_virtual(spi, "engineDoFinal", "()[B", &[])?;
    let bytes = match produced {
        Some(Value::Object(Some(a))) => {
            let n = scope.array_length(a);
            let mut b = vec![0u8; n];
            scope.read_byte_array_into(a, 0, &mut b);
            b
        }
        _ => Vec::new(),
    };
    let out = scope.get(&out_h);
    if off + bytes.len() > scope.array_length(out) {
        return Err(crate::phases_early::throw_jca_exc(
            &mut *scope,
            "javax/crypto/ShortBufferException",
            "Cannot store MAC in output buffer",
        ));
    }
    scope.write_byte_array_from(out, off, &bytes);
    let spi_now = scope.get(&spi_h);
    scope.invoke_virtual(spi_now, "engineReset", "()V", &[])?;
    Ok(None)
}

pub fn register_p68_crypto_mac(r: &mut NativeMethodRegistry) {
            let mac = "javax/crypto/Mac";
    r.register(
        mac,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Mac;",
        |ctx, args| {
            // SBR wave 2: `--real-jdk` mirrors the JDK's provider list first
            // (an application's `Security.addProvider` is real bytecode there);
            // a no-op otherwise. See `provider_chain::sync_chain_from_real`.
            let fresh = crate::jca::provider_chain::sync_chain_then(ctx, args);
            let args = &fresh[..];
            // The algorithm String is the first reference arg. State lives
            // off-object in mac_state_table, keyed by identity hash (bug-26 L3).
            // See `mac_algorithm_arg` for why the scan is a scan.
            let Some((_, algo)) = mac_algorithm_arg(ctx, args) else {
                // HotSpot: `Mac.getInstance(null)` is
                // `NullPointerException: null algorithm name` — measured.
                return Err(RuntimeError::NullPointerException {
                    message: Some("null algorithm name".to_string()),
                }
                .into());
            };
            // An `Alg.Alias.Mac.<oid>` spelling resolves to the primary name
            // first — but only through a provider this VM implements. A
            // THIRD-PARTY provider's alias row names that provider's own MAC,
            // not a rename this engine may answer; see
            // `provider_chain::canonical_if_unrecognised_native_only`. The
            // anonymous overload then searches the chain in chain order below,
            // which is what hands such a name to its owner.
            let algo = crate::jca::provider_chain::canonical_if_unrecognised_native_only_in(
                ctx.vm_identity(),
                "Mac",
                &algo,
                &mac_algorithm_supported,
            )
            .unwrap_or(algo);
            // A name this engine cannot compute is not automatically a missing
            // algorithm: the ANONYMOUS overload promised "whatever the chain
            // gives me". Ask the installed providers first, exactly as the
            // provider-taking overload below already does — without this,
            // `Mac.getInstance("1.3.14.3.2.26")` (HMAC-SHA1 by OID, which is how
            // BouncyCastle's `JcePKCS12MacCalculatorBuilder` asks) refused while
            // BouncyCastle implements it and HotSpot serves it.
            if !mac_algorithm_supported(&algo) {
                if let Some(p) = crate::jca::provider_chain::find_service_provider_in(
                    ctx.vm_identity(),
                    "Mac",
                    &algo,
                ) {
                    if let Some(obj) =
                        crate::jca::provider_chain::build_real_mac(ctx, &p, &algo, &algo)?
                    {
                        return Ok(Some(Value::Object(Some(obj))));
                    }
                }
            }
            // W4-3: refuse BEFORE allocating a receiver. An unimplemented name
            // used to yield a working-looking Mac that computed HMAC-SHA-256
            // under whatever name the caller asked for — see
            // `mac_algorithm_supported`.
            if !mac_algorithm_supported(&algo) {
                return Err(mac_no_such_algorithm(ctx, &algo, None));
            }
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/Mac", 4)?;
            let id = mac_state_key(ctx, obj);
            // BUG nb-phases-late(4): bound the key-bearing side-table before
            // inserting so it cannot retain key material for the VM lifetime.
            let mut t = mac_state_table().lock().unwrap();
            mac_state_evict_for_vm_if_needed(&mut t, id);
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
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Both provider-taking overloads. `getInstance(String, Provider)` was never
    // registered, so it fell through to the real `Mac.getInstance` bytecode and
    // built a receiver whose state this crate's `init`/`update`/`doFinal` do not
    // manage. `check_named_provider_arg` and `provider_arg_name` already
    // discriminate the two argument shapes by the argument's own class, so one
    // body serves both.
    for desc in [
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Mac;",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;",
    ] {
        r.register(mac, "getInstance", desc, |ctx, args| {
            let fresh = crate::jca::provider_chain::sync_chain_then(ctx, args);
            let args = &fresh[..];
            // State lives off-object in mac_state_table, keyed by identity hash
            // (bug-26 L3).
            //
            // W4-3: locate the algorithm first, then take the provider as the
            // argument immediately AFTER it rather than at a fixed index — that
            // is what makes this correct under both of the tree's static-native
            // argument conventions (see `mac_algorithm_arg`).
            let Some((algo_idx, algo)) = mac_algorithm_arg(ctx, args) else {
                return Err(RuntimeError::NullPointerException {
                    message: Some("null algorithm name".to_string()),
                }
                .into());
            };
            // Real JDK resolves the PROVIDER before the algorithm, so an
            // unregistered provider is `NoSuchProviderException` and an empty
            // one `IllegalArgumentException("missing provider")` — never a
            // `NoSuchAlgorithmException`. `check_named_provider_arg` is the
            // shared implementation of that ordering; this overload previously
            // discarded the provider argument entirely, so asking a provider
            // that was never registered quietly succeeded.
            crate::jca::provider_chain::check_named_provider_arg(
                ctx,
                args,
                algo_idx + 1,
                crate::jca::provider_chain::ProviderArgWording::Shared,
            )?;
            // Provider EXISTENCE is not provider OWNERSHIP, and this overload
            // used to stop at the first. Measured on HotSpot 25.0.3+9 today:
            //
            // ```text
            // Mac.getInstance("HmacSHA256", "SUN")
            //   -> NoSuchAlgorithmException: no such algorithm: HmacSHA256 for provider SUN
            // ```
            //
            // `SUN` is a registered provider with **zero** `Mac` service rows
            // (`SUN.getServices()` filtered to type `Mac` is empty — measured),
            // so `check_named_provider_arg` admits it and every name in
            // `mac_algorithm_supported` then answered a working HMAC under a
            // provider that does not supply MACs at all. That is the
            // wrong-accept half of the W4-3 species: not a wrong MAC, but a MAC
            // where HotSpot refuses, which is the shape that only ever presents
            // as an interop bug against everyone running a real JDK.
            //
            // `check_provider_ownership` consults the same service table
            // `jca::provider_chain` seeds and that `Provider.put` populates, so
            // an application provider registered via `Security.addProvider` +
            // `Provider.put("Mac.<algo>", …)` is still owned and still served.
            // Its `Shared` wording is byte-identical to the measured message.
            crate::jca::provider_chain::check_provider_ownership(
                ctx,
                args,
                algo_idx + 1,
                "Mac",
                &algo,
                crate::jca::provider_chain::ProviderArgWording::Shared,
            )?;
            // Resolve against the NAMED provider's own alias rows before the
            // engine's name gate — see the anonymous overload above.
            let requested_provider =
                crate::jca::provider_chain::provider_arg_name(ctx, args, algo_idx + 1);
            let requested_algo = algo.clone();
            let algo = crate::jca::provider_chain::canonical_if_unrecognised_in(
                ctx.vm_identity(),
                requested_provider.as_deref(),
                "Mac",
                &algo,
                &mac_algorithm_supported,
            )
            .unwrap_or(algo);
            // A caller that NAMED a third-party provider gets THAT provider's
            // `MacSpi`, in a genuine `javax.crypto.Mac` — see
            // `mac_delegate_spi`. This is both an attribution fix (HotSpot
            // answers `BC`, this VM answered `SunJCE`) and a capability one:
            // the BC-only MAC families have no arm in `mac_compute_hmac` at
            // all, so `getInstance` refused names the named provider
            // implements.
            if let Some(provider) = requested_provider.as_deref() {
                if let Some(obj) = crate::jca::provider_chain::build_real_mac(
                    ctx,
                    provider,
                    &requested_algo,
                    &algo,
                )? {
                    return Ok(Some(Value::Object(Some(obj))));
                }
            }
            // No provider named and this engine cannot serve the name: fall to
            // the chain, whose only candidates for a name we do not implement
            // are third-party providers. `1.3.14.3.2.26` (SHA-1 HMAC by OID,
            // bc-java's `pkcs` suite) is the shape.
            if requested_provider.is_none() && !mac_algorithm_supported(&algo) {
                if let Some(p) = crate::jca::provider_chain::find_service_provider_in(
                    ctx.vm_identity(),
                    "Mac",
                    &algo,
                ) {
                    if let Some(obj) =
                        crate::jca::provider_chain::build_real_mac(ctx, &p, &requested_algo, &algo)?
                    {
                        return Ok(Some(Value::Object(Some(obj))));
                    }
                }
            }
            if !mac_algorithm_supported(&algo) {
                // Once a provider has been named, HotSpot reports the failure
                // against THAT provider: `no such algorithm: X for provider Y`.
                return Err(mac_no_such_algorithm(
                    ctx,
                    &algo,
                    requested_provider.as_deref(),
                ));
            }
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/Mac", 4)?;
            let id = mac_state_key(ctx, obj);
            // BUG nb-phases-late(4): bound the key-bearing side-table before
            // inserting so it cannot retain key material for the VM lifetime.
            let mut t = mac_state_table().lock().unwrap();
            mac_state_evict_for_vm_if_needed(&mut t, id);
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
        }, cratonvm_native_api::NativeKind::Bridge);
        // getInstance(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;
        //
        // The SEVENTEENTH public method of `javax.crypto.Mac`, and the one that was
        // registered nowhere. It went missing because
        // `phases_late::every_public_mac_method_is_registered` — the census whose
        // doc comment says "Every PUBLIC method of `javax.crypto.Mac` must be
        // registered — not most of them" — had its population transcribed from THIS
        // registrar rather than from the JDK, so it listed 16 of 17 and every one of
        // the 16 passed. See
        // `docs/feature-designs/native-dispatch-architecture.md`.
        //
        // Leaving it unregistered is not inert. The other two overloads allocate a
        // 4-slot synthetic `javax/crypto/Mac` and seed `mac_state_table`; this one
        // fell through to the REAL JDK body, which hands back a `Mac` with no
        // `mac_state_table` row — while `init`, `update`, `doFinal`, `getAlgorithm`,
        // `getMacLength` and `reset` on that object are ALL intercepted by natives
        // that read that row. `getMacLength()` on it answers
        // `IllegalStateException: MAC algorithm unavailable:` (empty name), and
        // `doFinal()` answers on an empty key — a silently WRONG MAC, which is the
        // worst outcome in this file's own accounting, since a MAC that verifies is
        // itself the security decision.
        //
        // ## The contract, measured on HotSpot 25.0.3+9 (this host, 2026-08-13)
        //
        // ```text
        // getInstance("HmacSHA256", SunJCE)      -> OK  prov=SunJCE len=32, getProvider() == the SAME instance
        // getInstance("HmacSHA256", SUN)         -> NoSuchAlgorithmException: no such algorithm: HmacSHA256 for provider SUN
        // getInstance("HmacSHA256", new Provider("MyAnon",…){})
        //                                        -> NoSuchAlgorithmException: no such algorithm: HmacSHA256 for provider MyAnon
        // getInstance("NoSuchMac",  SunJCE)      -> NoSuchAlgorithmException: no such algorithm: NoSuchMac for provider SunJCE
        // getInstance("",           SunJCE)      -> NoSuchAlgorithmException: no such algorithm:  for provider SunJCE
        // getInstance("HmacSHA256", (Provider)null) -> IllegalArgumentException: missing provider
        // getInstance(null,         SunJCE)      -> NullPointerException: null algorithm name
        // getInstance(null,         (Provider)null) -> NullPointerException: null algorithm name
        // ```
        //
        // Three rows are the whole design, and none is guessable from the
        // `(String, String)` overload:
        //
        // 1. **There is no `NoSuchProviderException` on this overload at all.** The
        //    caller handed over a `Provider` INSTANCE, so there is nothing to look
        //    up; a provider that was never passed to `Security.addProvider` is
        //    perfectly acceptable as an argument (`MyAnon` above got as far as the
        //    algorithm check). `check_named_provider_arg` is called anyway and is a
        //    documented no-op for a non-`String` argument — it is here for the null
        //    row, which it does handle, and so that the two overloads keep one
        //    ordering rather than two.
        // 2. **A null provider is `IllegalArgumentException("missing provider")`,
        //    not NPE** — the same `IllegalArgumentException` the `(String, String)`
        //    overload gives for `null` and for `""`.
        // 3. **The null-algorithm check runs FIRST.** `getInstance(null, null)` is
        //    NPE, not IAE — measured on both two-argument overloads. So the
        //    algorithm argument is located before the provider is examined, which is
        //    exactly the order below.
        //
        // Ownership then decides the rest: whether the *named* provider supplies the
        // algorithm is the only question this overload can fail on, and HotSpot
        // answers it identically for a registered provider that lacks the row (SUN)
        // and an unregistered one that lacks it (MyAnon). `check_provider_ownership`
        // reads the same service table for both and `ProviderArgWording::Shared`
        // reproduces the message verbatim.
        //
        // **Known residual, stated so a green run is not read as more than it is:**
        // on the SYNTHETIC path only — a name this engine computes itself —
        // `getProvider()` answers the canonical `SunJCE` object
        // (`jce_provider_object`), not the instance the caller passed. HotSpot
        // returns the caller's own instance (`m.getProvider() == provider` measured
        // `true`); this VM answers an equal NAME but a different object. Fixing
        // that means giving `MacState` a provider field, and `MacState` is built
        // with an explicit all-fields literal in `phases_late.rs` — a file this
        // lane does not own — whose own comment says to list every field.
        //
        // A Mac built from the named provider's own SPI (`build_real_mac`, below)
        // does not have this residual: it is constructed through
        // `javax.crypto.Mac`'s real `(MacSpi, Provider, String)` constructor with
        // that provider's object, so `getProvider().getName()` is `BC`.
        r.register(
            mac,
            "getInstance",
            "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;",
            |ctx, args| {
                let fresh = crate::jca::provider_chain::sync_chain_then(ctx, args);
                let args = &fresh[..];
                // NOT `mac_algorithm_arg`: that one takes the first argument
                // `read_string` succeeds on, and on THIS overload the second
                // argument is a `Provider`, whose `read_string` is not reliably
                // `None`. With a null algorithm the plain scan would hand the
                // PROVIDER back as the algorithm name and answer
                // `NoSuchAlgorithmException` where HotSpot answers
                // `NullPointerException: null algorithm name`. See
                // `mac_algorithm_arg_string_typed`.
                let Some((algo_idx, algo)) = mac_algorithm_arg_string_typed(ctx, args) else {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("null algorithm name".to_string()),
                    }
                    .into());
                };
                // Null provider => IllegalArgumentException("missing provider").
                // A non-null, non-String argument is the Provider instance itself,
                // which this call is a documented no-op for.
                crate::jca::provider_chain::check_named_provider_arg(
                    ctx,
                    args,
                    algo_idx + 1,
                    crate::jca::provider_chain::ProviderArgWording::Shared,
                )?;
                // The one question this overload can fail on: does THAT provider
                // supply this algorithm? `check_provider_ownership` reads the
                // provider's name off the instance via `provider_name_of` when the
                // argument is not a String, so an unregistered `Provider` object is
                // reported by its own name — which is what HotSpot does.
                crate::jca::provider_chain::check_provider_ownership(
                    ctx,
                    args,
                    algo_idx + 1,
                    "Mac",
                    &algo,
                    crate::jca::provider_chain::ProviderArgWording::Shared,
                )?;
                // Resolve against the NAMED provider's own alias rows, then build
                // that provider's own `MacSpi` — the same two steps the
                // `(String, String)` overload takes, for the same reasons, and this
                // overload had NEITHER.
                //
                // Ownership was checked just above and then the name was put to
                // THIS engine's `mac_algorithm_supported` gate, so every algorithm
                // BouncyCastle owns and this engine does not compute was refused
                // with `no such algorithm: <name> for provider BC` — a provider
                // being told it does not implement what it had just been confirmed
                // to own, one line earlier, by the same table.
                //
                // Measured against HotSpot 25 (`MacProvObj2`), before:
                //
                // ```text
                // 1.3.14.3.2.26     byName   BC len=20 2376178e…
                //                   byObject EX NoSuchAlgorithmException:
                //                            no such algorithm: 1.3.14.3.2.26 for provider BC
                // ```
                //
                // `PBEwithHmacSHA1` and `AESCMAC` are the same shape. HotSpot
                // serves both forms identically, which is what makes the two
                // overloads' disagreement the defect rather than a policy: a caller
                // holding a `Provider` INSTANCE — which is what
                // `Security.getProvider("BC")` hands back, and what BouncyCastle's
                // own `JcaJceHelper`/`PKCS12` paths carry — got a refusal the same
                // caller would not have got from the string.
                let requested_provider =
                    crate::jca::provider_chain::provider_arg_name(ctx, args, algo_idx + 1);
                let requested_algo = algo.clone();
                let algo = crate::jca::provider_chain::canonical_if_unrecognised_in(
                    ctx.vm_identity(),
                    requested_provider.as_deref(),
                    "Mac",
                    &algo,
                    &mac_algorithm_supported,
                )
                .unwrap_or(algo);
                if let Some(provider) = requested_provider.as_deref() {
                    if let Some(obj) = crate::jca::provider_chain::build_real_mac(
                        ctx,
                        provider,
                        &requested_algo,
                        &algo,
                    )? {
                        return Ok(Some(Value::Object(Some(obj))));
                    }
                }
                // Backstop for the case ownership cannot see: a provider whose name
                // this VM could not read (`provider_name_of` -> "<unknown>", which
                // `check_provider_ownership` deliberately admits) asking for a name
                // this engine does not compute. Refusing BEFORE allocating is the
                // W4-3 rule — an unimplemented name must never reach a receiver,
                // because `mac_compute_hmac` has no default arm and the object would
                // be a Mac that answers for an algorithm nothing here implements.
                if !mac_algorithm_supported(&algo) {
                    // Once a provider is in play HotSpot reports the failure against
                    // THAT provider, so the message must carry its name.
                    let provider = match args.get(algo_idx + 1) {
                        Some(Value::Object(Some(p))) => {
                            Some(crate::jca::provider_chain::provider_name_of(ctx, *p))
                        }
                        _ => None,
                    };
                    return Err(mac_no_such_algorithm(ctx, &algo, provider.as_deref()));
                }
                let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/Mac", 4)?;
                let id = mac_state_key(ctx, obj);
                // BUG nb-phases-late(4): bound the key-bearing side-table before
                // inserting so it cannot retain key material for the VM lifetime.
                let mut t = mac_state_table().lock().unwrap();
                mac_state_evict_for_vm_if_needed(&mut t, id);
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
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    r.register(mac, "init", "(Ljava/security/Key;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            return ctx.invoke_virtual(
                spi,
                "engineInit",
                "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
                &[key, Value::Object(None)],
            );
        }
        // `Mac.init(null)` is `InvalidKeyException: No installed provider
        // supports this key: (null)`. This VM read the null as an EMPTY key,
        // marked the Mac initialised and reported success, so the first
        // `doFinal` returned a MAC computed under a zero-length key rather
        // than refusing. Below the SPI branch above on purpose: a third-party
        // SPI decides for itself.
        // MEASURED, `L6JcaSweep` row 124.
        if matches!(args.get(1), None | Some(Value::Object(None))) {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/InvalidKeyException",
                "No installed provider supports this key: (null)",
            ));
        }
        let key_bytes = match args.get(1) {
            Some(Value::Object(Some(k))) => mac_extract_key_bytes(ctx, *k),
            _ => Vec::new(),
        };
        // gc-common w15-d / w27-a: `init` can create the row (`or_default`);
        // the key is minted here, while `this` is current, and the row goes
        // with it.
        let id = mac_state_key(ctx, this);
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
    }, cratonvm_native_api::NativeKind::Bridge);
    // update([B)V — append byte array to accumulator
    r.register(mac, "update", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            let (arr, len) = match args.get(1) {
                Some(Value::Object(Some(a))) => (Some(*a), ctx.array_length(*a) as i32),
                _ => (None, 0),
            };
            return ctx.invoke_virtual(
                spi,
                "engineUpdate",
                "([BII)V",
                &[Value::Object(arr), Value::Int(0), Value::Int(len)],
            );
        }
        if let Some(Value::Object(Some(arr))) = args.get(1) {
            let bytes = mac_read_byte_array(ctx, *arr);
            let id = mac_state_key(ctx, this);
            mac_state_table()
                .lock()
                .unwrap()
                .entry(id)
                .or_default()
                .data
                .extend_from_slice(&bytes);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // update([BII)V — append byte range to accumulator
    r.register(mac, "update", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // JDK 25 `Mac.update(byte[], int, int)` refuses a bad range of a
        // non-null array with `IllegalArgumentException("Bad arguments")`
        // before the SPI sees it (wave 100 L7: the delegated path passed it on
        // and the local one raised an AIOOBE "Array index out of range: n").
        if let Some(Value::Object(Some(arr))) = args.get(1) {
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            let arr_len = ctx.array_length(*arr) as i64;
            if off < 0 || i64::from(len) > arr_len - i64::from(off) || len < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Bad arguments".to_string(),
                }
                .into());
            }
        }
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => Some(*a),
                _ => None,
            };
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            return ctx.invoke_virtual(
                spi,
                "engineUpdate",
                "([BII)V",
                &[Value::Object(arr), Value::Int(off), Value::Int(len)],
            );
        }
        if let Some(Value::Object(Some(arr))) = args.get(1) {
            // Validate signed off/len against the array length BEFORE casting to
            // usize. A negative len would sign-extend into a huge usize and
            // abort `Vec::with_capacity`; reject out-of-range ranges with
            // IndexOutOfBoundsException as the JDK Mac/SPI does.
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            let arr_len = ctx.array_length(*arr) as i64;
            if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
                // Refused above with the JDK's text; kept as the guard the
                // `usize` casts below rely on.
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Bad arguments".to_string(),
                }
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
            let id = mac_state_key(ctx, this);
            mac_state_table()
                .lock()
                .unwrap()
                .entry(id)
                .or_default()
                .data
                .extend_from_slice(&bytes);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // update(B)V — append single byte to accumulator
    r.register(mac, "update", "(B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            let b = args.get(1).copied().unwrap_or(Value::Int(0));
            return ctx.invoke_virtual(spi, "engineUpdate", "(B)V", &[b]);
        }
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        let id = mac_state_key(ctx, this);
        mac_state_table()
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .data
            .push(b);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // doFinal()[B — compute HMAC, return result, reset accumulator
    r.register(mac, "doFinal", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            return mac_delegate_do_final(ctx, spi, None);
        }
        let id = mac_state_key(ctx, this);
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
            // `getInstance` refuses every name `mac_compute_hmac` cannot serve,
            // so `None` means the side-table entry was rebuilt by `or_default()`
            // or evicted, i.e. the algorithm was lost — NOT that the caller
            // asked for something exotic. Raise rather than emit HMAC-SHA-256
            // under whatever name happens to be on file.
            match mac_compute_hmac(&st.algo, &st.key, &data) {
                Some(bytes) => bytes,
                None => {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!("MAC algorithm unavailable: {}", st.algo),
                    }
                    .into());
                }
            }
        };
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, hmac_result.len());
        ctx.write_byte_array_from(arr, 0, &hmac_result);
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // doFinal([B)[B — update with input bytes, then compute HMAC
    r.register(mac, "doFinal", "([B)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            let input = match args.get(1) {
                Some(Value::Object(Some(a))) => Some(*a),
                _ => None,
            };
            return mac_delegate_do_final(ctx, spi, input);        }
        let id = mac_state_key(ctx, this);
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
            // See the `doFinal()[B` arm above for why `None` raises here.
            match mac_compute_hmac(&st.algo, &st.key, &data) {
                Some(bytes) => bytes,
                None => {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!("MAC algorithm unavailable: {}", st.algo),
                    }
                    .into());
                }
            }
        };
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, hmac_result.len());
        ctx.write_byte_array_from(arr, 0, &hmac_result);
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // doFinal([BI)V — compute the MAC into the caller's buffer at `outOffset`.
    //
    // THIS OVERLOAD WAS MISSING, and its absence is what broke every
    // SCRAM-SHA-256 login on this VM (hibernate / Vert.x reactive
    // Postgres: `FATAL: expected SASL response, got message type 88`, where 88
    // is 'X' — the client giving up and sending Terminate).
    //
    // The failure mode is the trap this whole module is exposed to: `Mac` is
    // served by natives that keep their state OFF-OBJECT in `mac_state_table`,
    // so the real `javax.crypto.Mac` instance fields — `initialized`, `spi`,
    // `provider` — are never written. Any overload left unregistered therefore
    // runs the REAL JDK body, reads `initialized == false`, and throws
    // `IllegalStateException("MAC not initialized")` on a Mac that this engine
    // considers perfectly initialized. It is not a missing feature that reads
    // as missing: `init`, `update` and `doFinal()` all work, so the object
    // looks healthy right up to the one call that doesn't.
    //
    // `com.ongres.scram.common.CryptoUtil.hi` (the PBKDF2 inside SCRAM, used by
    // BOTH the Vert.x and pgjdbc SCRAM clients) does exactly
    // `update(byte[]); doFinal(byte[], 0)` in its iteration loop, so it hit this
    // on iteration 2 of 4096 — after `doFinal()` had already succeeded once.
    r.register(mac, "doFinal", "([BI)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            return mac_delegate_do_final_into(ctx, spi, args);
        }
        let id = mac_state_key(ctx, this);
        let out = match args.get(1) {
            Some(Value::Object(Some(arr))) => *arr,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("Cannot store MAC in output buffer".to_string()),
                }
                .into());
            }
        };
        let out_offset = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let out_len = ctx.array_length(out) as i64;
        // `short_buffer` is carried OUT of the locked block rather than thrown
        // inside it: building the exception runs a Java constructor, and this
        // module's rule is that no `ctx` call happens while the state mutex is
        // held (`std::sync::Mutex` is not reentrant, and a constructor can
        // reach back into a Mac native).
        let mut short_buffer = false;
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
            // The JDK checks the output buffer AFTER the initialized check and
            // BEFORE consuming the accumulator, and a ShortBufferException
            // leaves the Mac's buffered data intact — so this must not
            // `mem::take` before the size test.
            let mac_len = match mac_output_length(&st.algo) {
                Some(n) => n as i64,
                None => {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!("MAC algorithm unavailable: {}", st.algo),
                    }
                    .into());
                }
            };
            if out_offset < 0 || out_len - (out_offset as i64) < mac_len {
                short_buffer = true;
                Vec::new()
            } else {
                // mem::take resets the accumulator (keeps Mac initialized for reuse).
                let data = std::mem::take(&mut st.data);
                match mac_compute_hmac(&st.algo, &st.key, &data) {
                    Some(bytes) => bytes,
                    None => {
                        return Err(RuntimeError::IllegalStateException {
                            message: format!("MAC algorithm unavailable: {}", st.algo),
                        }
                        .into());
                    }
                }
            }
        };
        if short_buffer {
            // Real, checked `javax.crypto.ShortBufferException` — a
            // `RuntimeError` variant would be unchecked and would sail past
            // the caller's `catch (ShortBufferException)`.
            return Err(mac_short_buffer(ctx, "Cannot store MAC in output buffer"));
        }
        ctx.write_byte_array_from(out, out_offset as usize, &hmac_result);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // init(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V —
    // same reason as `doFinal([BI)V`: leaving it to the real body means a
    // null `spi`/`lock`, not a working two-argument init. No HMAC algorithm
    // this module serves takes parameters, so the spec is accepted and
    // ignored exactly as `HmacCore.engineInit` does for a null spec.
    r.register(
        mac,
        "init",
        "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Its one-argument sibling routes a provider-delegated Mac to the
            // SPI; this overload did not, so a BouncyCastle `Mac` initialised
            // with a parameter spec never had `engineInit` called at all. The
            // symptom is downstream and names the wrong layer: BC's own
            // lightweight engine reports `IllegalStateException: DESede engine
            // not initialised` / `GCM cipher needs to be initialised` /
            // `CCM cipher unitialized` at the first `update`, which reads as a
            // BouncyCastle bug rather than a dropped `init`. bc-java's whole
            // `NewAuthenticatedDataTest` family failed that way.
            if let Some(spi) = mac_delegate_spi(ctx, this) {
                let key = args.get(1).copied().unwrap_or(Value::Object(None));
                let spec = args.get(2).copied().unwrap_or(Value::Object(None));
                return ctx.invoke_virtual(
                    spi,
                    "engineInit",
                    "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
                    &[key, spec],
                );
            }
            let key_bytes = match args.get(1) {
                Some(Value::Object(Some(k))) => mac_extract_key_bytes(ctx, *k),
                _ => Vec::new(),
            };
            let id = mac_state_key(ctx, this);
            let mut t = mac_state_table().lock().unwrap();
            let st = t.entry(id).or_default();
            mac_zeroize(&mut st.key);
            st.key = key_bytes;
            st.initialized = true;
            st.data.clear();
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // update(Ljava/nio/ByteBuffer;)V — consume the buffer's remaining bytes and
    // leave its position at the limit, which is what the JDK contract says and
    // what `Mac.update(ByteBuffer)` callers rely on. Draining through the
    // buffer's own `get(byte[])` keeps heap and DIRECT buffers on the same
    // path; reading `hb` directly would silently no-op on a direct buffer.
    r.register(mac, "update", "(Ljava/nio/ByteBuffer;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            let buf = args.get(1).copied().unwrap_or(Value::Object(None));
            if matches!(buf, Value::Object(None)) {
                return Ok(None);
            }
            return ctx.invoke_virtual(spi, "engineUpdate", "(Ljava/nio/ByteBuffer;)V", &[buf]);
        }
        let Some(Value::Object(Some(buf))) = args.get(1).cloned() else {
            // JDK: a null ByteBuffer is a silent no-op (`if (input == null) …`).
            return Ok(None);
        };
        // gc-common w15-d: `remaining()` and `get(byte[])` are Java. The
        // receiver (keyed below), the buffer (`get`'s receiver) and the
        // drain array (read after `get`) were all used through their
        // pre-call addresses; the old pin on the array was released without
        // ever being read back.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let buf_h = scope.root(buf);
        let remaining = match scope.invoke_virtual(buf, "remaining", "()I", &[]) {
            Ok(Some(Value::Int(n))) if n > 0 => n as usize,
            _ => return Ok(None),
        };
        let tmp_new = scope.new_array(cratonvm_types::ArrayElementType::Byte, remaining);
        let tmp_h = scope.root(tmp_new);
        let buf_now = scope.get(&buf_h);
        scope.invoke_virtual(
            buf_now,
            "get",
            "([B)Ljava/nio/ByteBuffer;",
            &[Value::Object(Some(tmp_new))],
        )?;
        let tmp_now = scope.get(&tmp_h);
        let bytes = mac_read_byte_array(&mut *scope, tmp_now);
        let this_now = scope.get(&this_h);
        let id = mac_state_key(&*scope, this_now);
        drop(scope);
        mac_state_table()
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .data
            .extend_from_slice(&bytes);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // getProvider()Ljava/security/Provider; — the real body is
    // `chooseFirstProvider(); return provider;`, and `chooseFirstProvider`
    // opens with `synchronized (lock)` on a field this synthetic never wrote:
    // `NullPointerException: Cannot enter synchronized block because
    // "this.lock" is null`. Answer with the provider these HMACs actually
    // come from, which is what `Security.getProviders()` advertises them under.
    r.register(
        mac,
        "getProvider",
        "()Ljava/security/Provider;",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            if let Value::Object(Some(p)) = ctx.get_field_by_name(_this, "provider") {
                let pid = ctx.class_id_by_name("java/security/Provider");
                if pid.is_some_and(|pid| ctx.is_subclass(ctx.class_id_of_object(p), pid)) {
                    return Ok(Some(Value::Object(Some(p))));
                }
            }
            Ok(Some(Value::Object(Some(jce_provider_object(ctx)?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // reset()V — clear the accumulator
    r.register(mac, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            return ctx.invoke_virtual(spi, "engineReset", "()V", &[]);
        }
        let id = mac_state_key(ctx, this);
        if let Some(st) = mac_state_table().lock().unwrap().get_mut(&id) {
            st.data.clear();        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(mac, "getMacLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            return ctx.invoke_virtual(spi, "engineGetMacLength", "()I", &[]);
        }
        let id = mac_state_key(ctx, this);
        let algo = mac_state_table()
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.algo.clone())
            .unwrap_or_default();
        // `_ => 32` used to live in `mac_output_length`, which made this
        // accessor corroborate the fabricated HMAC-SHA-256 output for every
        // unimplemented algorithm. `getInstance` now refuses those names, so
        // `None` here means the side-table entry is gone — the same lost-state
        // condition `doFinal` reports as `IllegalStateException`.
        match mac_output_length(&algo) {
            Some(n) => Ok(Some(Value::Int(n as i32))),
            None => Err(RuntimeError::IllegalStateException {
                message: format!("MAC algorithm unavailable: {algo}"),
            }
            .into()),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(mac, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if mac_delegate_spi(ctx, this).is_some() {
            if let Value::Object(Some(a)) = ctx.get_field_by_name(this, "algorithm") {
                if ctx
                    .class_name_of_id(ctx.class_id_of_object(a))
                    .is_some_and(|n| n == "java/lang/String")
                {
                    return Ok(Some(Value::Object(Some(a))));
                }
            }
        }
        let id = mac_state_key(ctx, this);
        let algo = mac_state_table()
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.algo.clone())
            .unwrap_or_default();
        Ok(Some(Value::Object(Some(ctx.create_string(&algo)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(mac, "clone", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // A delegated Mac clones by cloning ITS spi and re-wrapping — the
        // synthetic clone below would silently hand back an unrelated,
        // natively-served Mac carrying none of the provider's state.
        if let Some(spi) = mac_delegate_spi(ctx, this) {
            // gc-common w15-d: `spi.clone()` is Java; the receiver whose
            // `provider` / `algorithm` are read after it is rooted across it.
            let this_pin = ctx.pin_native_root(this);
            let cloned = ctx.invoke_virtual(spi, "clone", "()Ljava/lang/Object;", &[]);
            let this_now = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            let Some(Value::Object(Some(cloned))) = cloned? else {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/CloneNotSupportedException",
                    "MacSpi is not cloneable",
                ));
            };
            let provider = ctx.get_field_by_name(this_now, "provider");
            let algorithm = ctx.get_field_by_name(this_now, "algorithm");
            let built = ctx.new_object_initialized(
                "javax/crypto/Mac",
                "(Ljavax/crypto/MacSpi;Ljava/security/Provider;Ljava/lang/String;)V",
                &[Value::Object(Some(cloned)), provider, algorithm],
            )?;
            return Ok(built);
        }
        let src_key = mac_state_key(ctx, this);
        let src_state = mac_state_table()
            .lock()
            .unwrap()
            .get(&src_key)
            .cloned()
            .unwrap_or_default();
        let clone = try_alloc_concurrent_synthetic(ctx, "javax/crypto/Mac", 4)?;
        let cid = mac_state_key(ctx, clone);
        mac_state_table().lock().unwrap().insert(cid, src_state);
        Ok(Some(Value::Object(Some(clone))))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

/// Locate the algorithm-name argument of a `Mac.getInstance` overload, as
/// `(index, name)`, or `None` when no reference argument is present at all.
///
/// **Why this is a scan and not `args.first()`.** The tree carries two
/// conventions for calling a static native and this lane could not run either
/// to settle it: `jca::message_digest::md_get_instance` reads `args.first()`
/// as the algorithm, and `vm/src/vm/tests.rs::crypto_mac_basics_p68` calls this
/// very native with a leading `Value::Object(None)` receiver placeholder and
/// the algorithm at index 1. The pre-existing scan is the only reading that
/// satisfies both, so it is preserved verbatim; changing it would red a test in
/// a file this lane does not own, and would buy nothing.
///
/// Returning the INDEX is the part that is new. The two-argument overload needs
/// the provider, and taking it from a fixed index 1 would have read the
/// *algorithm* as the provider under the placeholder convention — a
/// `NoSuchProviderException: no such provider: HmacSHA256` for a perfectly
/// valid call. Relative addressing has no such failure mode.
pub fn mac_algorithm_arg(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<(usize, String)> {
    args.iter().enumerate().find_map(|(i, v)| match v {
        Value::Object(Some(o)) => ctx.read_string(*o).map(|s| (i, s)),
        _ => None,
    })
}

/// `mac_algorithm_arg` for the `(String, Provider)` overload: locate the
/// algorithm by the argument's declared CLASS, not by whether `read_string`
/// happens to answer.
///
/// The plain scan takes the first argument `read_string` succeeds on. On the
/// `(String, String)` overload that is always the algorithm, because the
/// algorithm precedes the provider. On the `(String, Provider)` overload it is
/// only the algorithm while the algorithm is non-null: a `Provider` receiver is
/// not guaranteed to read back as `None` — `check_named_provider_arg` was
/// written around exactly that ("a Provider object that read back as an empty
/// string would otherwise be reported as a missing provider"). So with a null
/// algorithm the plain scan would return the PROVIDER as the algorithm name and
/// answer `NoSuchAlgorithmException`, where HotSpot 25.0.3+9 answers
/// `NullPointerException: null algorithm name` — measured, for both
/// `getInstance(null, SunJCE)` and `getInstance(null, (Provider)null)`.
///
/// The scan therefore skips any argument whose class is known and is not
/// `java/lang/String`, and falls back to `read_string` only for arguments whose
/// class the context cannot report at all. That fallback is deliberate: mock
/// contexts in this tree do not always answer `class_name_of_id`
/// (`docs/architecture/natives-over-real-jdk-classes.md` §4), and a helper that
/// returned `None` there would turn every mocked call into an NPE. Under the
/// fallback the behaviour degrades to exactly `mac_algorithm_arg`'s, never worse.
///
/// It also preserves the leading-`Value::Object(None)` receiver-placeholder
/// convention that `mac_algorithm_arg` documents: a `None` slot matches neither
/// branch and is skipped, so `[placeholder, algo, provider]` and
/// `[algo, provider]` both resolve, and the returned INDEX keeps the provider at
/// `idx + 1` under either.
pub fn mac_algorithm_arg_string_typed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<(usize, String)> {
    for (i, v) in args.iter().enumerate() {
        let Value::Object(Some(o)) = v else { continue };
        match ctx.class_name_of_id(ctx.class_id_of_object(*o)) {
            // Known to be the algorithm.
            Some(n) if n == "java/lang/String" => {
                return ctx.read_string(*o).map(|s| (i, s));
            }
            // Known NOT to be a String — this is the `Provider`. Skip it rather
            // than let `read_string` misreport it as the algorithm name.
            Some(_) => continue,
            // Class unknown: fall back to the original heuristic for this slot.
            None => {
                if let Some(s) = ctx.read_string(*o) {
                    return Some((i, s));
                }
            }
        }
    }
    None
}

/// Normalise a `Mac` algorithm name the way JCA lookup is case-insensitive.
///
/// `-` is stripped so `HMAC-SHA256` and `HmacSHA256` agree; `/` is deliberately
/// NOT stripped, because `HmacSHA512/224` and `HmacSHA512/256` are two DISTINCT
/// SunJCE algorithms (FIPS 180-4 §5.3.6 truncations with their own IVs) and
/// collapsing them would put serving one where the caller asked for the other
/// a single keystroke away. Both are implemented below, each over its own
/// digest — the `/` still must not be stripped, because `HMACSHA512224` would
/// then be indistinguishable from a `HmacSHA-512-224` spelling that means the
/// same thing but also from nothing else, and the two truncations differ only
/// by that suffix.
pub fn mac_normalise(algo: &str) -> String {
    algo.to_uppercase().replace('-', "")
}

/// The `Mac` algorithms this module can compute CORRECTLY — nothing else.
///
/// ## Why this predicate exists (W4-3 advertised-vs-implemented census)
///
/// `mac_compute_hmac` used to end in `_ => hmac_sha256(key, data)` and
/// `mac_output_length` in `_ => 32`, so **every** name outside the four
/// explicit arms was silently served as HMAC-SHA-256 — and `getMacLength()`
/// corroborated it by answering 32. Neither `getInstance` overload validated
/// anything, so this was reachable straight from application code. Measured on
/// jdk-25.0.3.9-hotspot, these are real algorithms a caller has every reason to
/// ask for:
///
/// * `Mac.getInstance("HmacSHA3-256")` — HotSpot: `len=32 prov=SunJCE` — got
///   HMAC-SHA-256 bytes here;
/// * `HmacSHA224` — HotSpot: `len=28` — got HMAC-SHA-256 bytes AND length 32,
///   so even the length disagreement raised no error;
/// * `HmacSHA512/224`, `HmacSHA512/256`, `Poly1305`, `AESCMAC`, and any typo:
///   all HMAC-SHA-256.
///
/// A wrong MAC is worse than a missing one, and worse than a wrong digest: a
/// MAC that verifies IS the security decision. Two peers both computing the
/// wrong-but-identical MAC interoperate happily, so the defect never presents
/// as "the crypto is wrong" — it presents as an interop bug against everyone
/// running a real JDK, if it presents at all.
///
/// ## Which names, and why these
///
/// The set started at five (`HmacMD5`, `HmacSHA1`, `HmacSHA256`, `HmacSHA384`,
/// `HmacSHA512`) — the names
/// `jca::provider_chain::seed_retired_getalgorithms_literals` registers as
/// `SunJCE` `Mac` services — with the note that widening it "means implementing
/// RFC 2104 over the digests `crate::compute_digest` already supplies … and
/// each needs its own HMAC block size … and this lane could neither build nor
/// run". `HmacSHA224` joined on 2026-08-12 (W7-39); the remaining six followed
/// the same day, from the hibernate SCRAM investigation.
///
/// The block size was the stated obstacle, and `hmac::Hmac<D>` removes it: it
/// reads the size from `D::BlockSize`, so none of 64 / 128 / 144 / 136 / 104 /
/// 72 is written down here at all. See `hmac_over_digest!`.
///
/// The pairing with the advertised list is a RATCHET, not a restatement:
/// `provider_chain::every_advertised_sunjce_mac_is_computable` derives the list
/// from the registry, so adding a row on either side without the other reds a
/// test. `mac_supported_set_matches_the_advertised_sunjce_services` restates it
/// on this side, and earns its place by pinning the normalisation the
/// registry-derived half cannot see: `mac_normalise` folds case and strips `-`
/// but keeps `/`, which is what stops `HmacSHA512/224` being served for
/// `HmacSHA512/256`.
///
/// `HmacSHA224` in particular is not academic: `com.ongres.scram` builds its
/// advertised mechanism list by probing `Mac.getInstance`, so refusing
/// `HmacSHA224` and `HmacSHA3-512` made CratonVM offer 8 SCRAM mechanisms where
/// HotSpot offers 12 — a protocol-level divergence against any server that
/// negotiates one of the four.
///
/// HotSpot 25 advertises 28 `Mac` names. Under-advertising 16 of them is
/// truthful precisely because `getInstance` refuses all 16, and they are all
/// deliberately still refused for one reason: `HmacPBESHA*` / `PBEWithHmac*`
/// are PKCS#12 and PBMAC1 constructions, and `Poly1305` / `AESCMAC` /
/// `SslMacSHA1` are not HMAC at all. Serving any of them would mean a second,
/// unverified construction — the mistake the `_ => hmac_sha256` fallback was
/// removed for.
pub fn mac_algorithm_supported(algo: &str) -> bool {
    // Derived from `mac_output_length` rather than restated, so the two cannot
    // disagree about which names exist.
    mac_output_length(algo).is_some()
}

/// `NoSuchAlgorithmException` for a `Mac` name we cannot compute, in HotSpot's
/// own wording.
///
/// Measured, not recalled: the one-argument overload answers `Algorithm
/// NO-SUCH-MAC not available`; the two-argument overload — after the provider
/// has resolved — answers `no such algorithm: NO-SUCH-MAC for provider SunJCE`.
/// `throw_no_such_algorithm_public` builds a genuine
/// `java/security/NoSuchAlgorithmException`, so a caller's
/// `catch (NoSuchAlgorithmException)` matches it. A
/// `RuntimeError::SecurityException` would be unchecked and would sail straight
/// past that handler — the mistake `jca::message_digest::md_get_instance`
/// records having made and corrected.
pub fn mac_no_such_algorithm(
    ctx: &mut dyn NativeContext,
    algo: &str,
    provider: Option<&str>,
) -> MethodCallFailed {
    let msg = match provider {
        Some(p) => format!("no such algorithm: {algo} for provider {p}"),
        None => format!("Algorithm {algo} not available"),
    };
    crate::jca::provider_chain::throw_no_such_algorithm_public(ctx, &msg)
}

/// Compute HMAC using the appropriate hash based on the Java algorithm name.
///
/// `None` means "this module does not implement that MAC". There is no default
/// arm, on purpose — see `mac_algorithm_supported` for what the default arm was
/// doing. `Mac.getInstance` refuses unsupported names before a receiver is ever
/// allocated, so `None` is unreachable through the public surface; the callers
/// therefore treat it as lost side-table state, not as a user error.
pub fn mac_compute_hmac(algo: &str, key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    match mac_normalise(algo).as_str() {
        "HMACSHA384" => Some(hmac_sha384(key, data)),
        "HMACSHA512" => Some(hmac_sha512(key, data)),
        "HMACSHA1" => Some(hmac_sha1(key, data)),
        "HMACMD5" => Some(hmac_md5(key, data)),
        "HMACSHA224" => Some(hmac_sha224(key, data)),
        "HMACSHA256" => Some(hmac_sha256(key, data)),
        // `/` survives `mac_normalise` on purpose: these two differ ONLY by the
        // suffix and have distinct initial values, so collapsing them would
        // serve one for the other.
        "HMACSHA512/224" => Some(hmac_sha512_224(key, data)),
        "HMACSHA512/256" => Some(hmac_sha512_256(key, data)),
        "HMACSHA3224" => Some(hmac_sha3_224(key, data)),
        "HMACSHA3256" => Some(hmac_sha3_256(key, data)),
        "HMACSHA3384" => Some(hmac_sha3_384(key, data)),
        "HMACSHA3512" => Some(hmac_sha3_512(key, data)),
        _ => None,
    }
}

// HMAC-SHA-224, and the six that followed it.
//
// The remaining four arms of `mac_compute_hmac` (MD5, SHA-1, SHA-256, SHA-384,
// SHA-512) route to `crate::hmac_*`, which pass a hand-written `block_size` to
// `crate::hmac_generic`. These do not, and the difference is the point:
// SHA-224's HMAC block is 64 bytes — its *input* block — not 28, its digest
// size, and not 128, which SHA-384/512 use. Every wrong answer there is
// plausible, and a wrong block size produces a MAC that is self-consistent and
// interoperates with nothing.
//
// `crate::compute_digest` already reaches for `sha2::Sha224` for
// `MessageDigest.getInstance("SHA-224")` for the reason its comment gives: the
// hand-rolled `crypto_impl` SHA-256 code does not cover the 224-bit variant.
// Using the same crates keeps `MessageDigest.SHA-224` and `Mac.HmacSHA224` on
// one implementation of one primitive — and the same for the SHA-3 family.
//
// Measured on jdk-25.0.3.9-hotspot, not recalled — see the vectors in
// `mac_kats_match_hotspot_25` and `hmac_extended_matches_hotspot`, each
// including a 200-byte key, which is the case that exercises the "key longer
// than the block gets hashed first" branch where a wrong block size first
// shows up.
hmac_over_digest!(hmac_sha224, sha2::Sha224);

hmac_over_digest!(hmac_sha512_224, sha2::Sha512_224);
hmac_over_digest!(hmac_sha512_256, sha2::Sha512_256);
hmac_over_digest!(hmac_sha3_224, sha3::Sha3_224);
hmac_over_digest!(hmac_sha3_256, sha3::Sha3_256);
hmac_over_digest!(hmac_sha3_384, sha3::Sha3_384);
hmac_over_digest!(hmac_sha3_512, sha3::Sha3_512);

/// Return the output length in bytes for the given HMAC algorithm, or `None`
/// for a name this module does not implement.
///
/// The retired `_ => 32` arm was the second half of the wrong-MAC defect: it
/// made `getMacLength()` agree with the fabricated HMAC-SHA-256 output, so a
/// caller sizing a buffer from `getMacLength()` saw a self-consistent — and
/// entirely wrong — engine. Kept in lockstep with `mac_compute_hmac`: every arm
/// here has an arm there and vice versa.
pub fn mac_output_length(algo: &str) -> Option<usize> {
    match mac_normalise(algo).as_str() {
        "HMACSHA384" => Some(48),
        "HMACSHA512" => Some(64),
        "HMACSHA1" => Some(20),
        "HMACMD5" => Some(16),
        // 28, not 32. The retired `_ => 32` arm answered 32 here while
        // `mac_compute_hmac` returned 32 SHA-256 bytes, so the two agreed with
        // each other and with nothing else — measured on HotSpot,
        // `Mac.getInstance("HmacSHA224").getMacLength()` is 28.
        "HMACSHA224" => Some(28),
        "HMACSHA256" => Some(32),
        "HMACSHA512/224" => Some(28),
        "HMACSHA512/256" => Some(32),
        "HMACSHA3224" => Some(28),
        "HMACSHA3256" => Some(32),
        "HMACSHA3384" => Some(48),
        "HMACSHA3512" => Some(64),
        _ => None,
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
pub const NEW13_SSL_CTX_FIELDS: usize = 5;

pub const NEW13_CTX_PROTOCOL: usize = 0;

pub const NEW13_CTX_INIT: usize = 1;

pub const NEW13_CTX_KM: usize = 2;

pub const NEW13_CTX_TM: usize = 3;

pub const NEW13_CTX_RANDOM: usize = 4;

// SSLSocket field layout (6 fields — matches previous register_p68_ssl, with
// the semantic of field 2 changed from fd_table fd_id → s2_registry tls_id).
//   0 = host String, 1 = port Int, 2 = tls_id Int (s2_registry),
//   3 = closed Int, 4 = session Object, 5 = reserved
pub const NEW13_SSL_SOCK_FIELDS: usize = 6;

pub const NEW13_SOCK_HOST: usize = 0;

pub const NEW13_SOCK_PORT: usize = 1;

pub const NEW13_SOCK_TLSID: usize = 2;

pub const NEW13_SOCK_CLOSED: usize = 3;

pub const NEW13_SOCK_SESSION: usize = 4;

/// Where `this`'s private slot map (`NEW13_SOCK_*`) starts: ABOVE the declared fields of an
/// `SSLSocketImpl` carrier (see `crate::tls_carriers`), `0` for the abstract-stamped carrier.
pub fn sock_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    crate::tls_carriers::carrier_base(ctx, this)
}

/// Read private slot `slot` of the socket carrier `this`.
pub fn sock_field(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    ctx.get_field(this, sock_base(ctx, this) + slot)
}

/// Write private slot `slot` of the socket carrier `this`.
pub fn sock_set_field(ctx: &dyn NativeContext, this: ObjectRef, slot: usize, value: Value) {
    ctx.set_field(this, sock_base(ctx, this) + slot, value);
}

/// JSSE's endpoint identification for a client `SSLSocket`, applied at `startHandshake`.
///
/// The VM connects (and handshakes) a client socket before the application can set `SSLParameters`, and the
/// connection's verifier skips the host-name check because JSSE makes it only when an endpoint identification
/// algorithm (`"HTTPS"` / `"LDAPS"`) is set (`X509TrustManagerImpl.checkIdentity` is skipped for `null`). When
/// the application set one, the peer's leaf is compared with the dialled host here and a mismatch is
/// `SSLHandshakeException`, as HotSpot's handshake fails. Witness:
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub fn new13_enforce_endpoint_identification(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let Some(alg_key) = existing_ssl_obj_key(&*ctx, this) else {
        return Ok(());
    };
    let Some(alg) = ssl_sock_endpoint_alg().lock().get(&alg_key).cloned() else {
        return Ok(());
    };
    if !(alg.eq_ignore_ascii_case("HTTPS") || alg.eq_ignore_ascii_case("LDAPS")) {
        return Ok(());
    }
    let tls_id = new13_resolve_tls_id(&*ctx, this);
    if tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE {
        return Ok(());
    }
    let Some(host) = crate::net_phase_e::sock_dialled_host(&*ctx, this) else {
        return Ok(());
    };
    let Some(chain) = crate::servlet::s2_tls_peer_cert_chain_der(tls_id) else {
        return Ok(());
    };
    match crate::x509_manager::check_endpoint_identity(&chain, &host) {
        Ok(()) => Ok(()),
        Err(_) => Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/net/ssl/SSLHandshakeException",
            // JSSE's `HostnameChecker` texts: an IP literal against a certificate with no IP SAN, else the
            // DNS-name form (`(certificate_unknown)` is the alert description JSSE prefixes).
            &if host.parse::<std::net::IpAddr>().is_ok() {
                "(certificate_unknown) No subject alternative names present".to_string()
            } else {
                format!("(certificate_unknown) No name matching {host} found")
            },
        )),
    }
}

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
/// netty-client-socket-write-after-close-nsme-FIXED.md).
pub fn new13_resolve_tls_id(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    if let Some(id) = sock_field(&*ctx, this, NEW13_SOCK_TLSID).as_int() {
        if id >= 0 {
            return id;
        }
    }
    crate::net_phase_e::sock_stream_id_for_upcall(ctx, this)
}

/// Has this `javax/net/ssl/SSLSocket` ever been successfully connected?
///
/// **E42 — this is a LATCH, not the negation of "closed", and that distinction
/// is measured rather than argued.** `isConnected()` used to answer
/// `!isClosed()`, which is wrong at BOTH ends. HotSpot 25.0.3+9-LTS, three
/// byte-identical runs (`scratchpad/e42/E42SocketPredicates.java`):
///
/// ```text
///   fresh SSLSocket (zero-arg createSocket)  isConnected=false isClosed=false
///   the same socket after close()            isConnected=false isClosed=true
///   connected, open                          isConnected=true  isClosed=false
///   connected, then close()                  isConnected=true  isClosed=true
///   bind()ed but never connected             isConnected=false isClosed=false
/// ```
///
/// So "not closed" answered `true` for the never-connected socket
/// (`RSslNullSession`'s DOOR 1 check 1, and the only check that vector
/// currently reaches) and `false` for a socket that was connected and then
/// closed. The two states are independent: `java.net.Socket` keeps them in
/// separate bits of one word — `CONNECTED = 1 << 2`, `CLOSED = 1 << 3`
/// (`jdk25src/java.base/java/net/Socket.java:115-157`), and `close()` sets
/// `CLOSED` without touching `CONNECTED`.
///
/// **The signals, in the order they are consulted, and why each is sound.**
/// This VM's `SSLSocket` has no `connected` bit of its own, so the latch is
/// reconstructed from state that already exists and that `close()` already
/// leaves alone:
///
/// 1. `new13_resolve_tls_id(..) >= 0` — a live stream. This is the same
///    question every I/O method on this socket asks, so a socket for which it
///    answers "no stream" cannot read or write either. Not a latch on its own:
///    `close()` stamps the id to `-1` (and `sock_mark_closed_for_upcall`
///    clears the side-table copy), which is exactly why it cannot be the whole
///    answer.
/// 2. `NEW13_SOCK_HOST` holding a String — the socket was given a peer. All
///    three connect paths write it (`connect`, `new13_finish_socket`, the
///    layered-handshake path), `SSLServerSocket.accept` writes the same slot,
///    and NOTHING clears it, including `close()`. The zero-arg
///    `createSocket()` never writes it. That is the latch.
/// 3. `net_phase_e`'s side-table `host` — the same signal for a socket built
///    through that module's own `createSocket(String, int)`, where the raw
///    field write may have been dropped by the layout guard (see
///    `new13_finish_socket`'s comment on why the side table is authoritative
///    for this class).
///
/// **This cannot introduce a new wrong answer.** Relative to `!closed` it can
/// only move a never-connected socket from `true` to `false` (correct) and a
/// closed-with-a-known-peer socket from `false` to `true` (correct). A
/// connected socket whose peer was recorded in neither place still answers
/// `false` after close — which is the answer it already gave.
pub fn new13_socket_ever_connected(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if new13_resolve_tls_id(ctx, this) >= 0 {
        return true;
    }
    if matches!(
        sock_field(&*ctx, this, NEW13_SOCK_HOST),
        Value::Object(Some(_))
    ) {
        return true;
    }
    !crate::net_phase_e::sock_get(ctx, this).host.is_empty()
}

// ---------------------------------------------------------------------------
// The receiver `SSLSocket.getInputStream()` / `getOutputStream()` hands back
// ---------------------------------------------------------------------------
//
// P1-C (`--real-jdk`). Until 2026-08-12 these two natives allocated
// `javax/net/ssl/SSLSocketInputStream` / `…OutputStream`. **No supported JDK
// image declares either name** — they are enumerated as such in
// `native-api/src/no_image_receiver.rs::NO_IMAGE_JDK_RECEIVERS`, which is what
// re-tags their natives `SyntheticStub`. Under `--real-jdk` the mint is
// refused, so the surviving `getInputStream` bridge ran, asked for a class §5
// forbids, and died as
//
//     java.lang.NoClassDefFoundError: javax/net/ssl/SSLSocketOutputStream
//
// at the application's first stream access — *after* a handshake that had
// genuinely succeeded. Net effect: no HTTPS at all under strict mode, client
// or server.
//
// The refusal was correct. The defect was the receiver, so the receiver is now
// a class the image really declares: the exact pair real JSSE's
// `SSLSocketImpl.getInputStream()`/`getOutputStream()` return, so
// `getClass().getName()` now agrees with HotSpot instead of naming a class
// HotSpot has never had. Present on every supported image (verified with
// `javap -p` against JDK 25 on this host; the pair has been nested inside
// `SSLSocketImpl` since the JDK 11 JSSE rewrite, so JDK 21 carries it too —
// that half is a reading, not a measurement).
//
// **This is the plain-socket precedent, and that precedent is sound.**
// `net_phase_e`'s `java/net/Socket.getInputStream()` allocates
// `java/net/Socket$SocketInputStream` — a real, image-declared nested class —
// and keeps its state in an identity-keyed side table *because* the real
// layout is not the native's layout ("Side-table the stream's owner+sid so we
// don't depend on field layout"). Both halves are carried over here: a real
// image class, and no native `Int` written over a field the real class
// declares.
//
// What is NOT carried over is the class name. Registering these natives on
// `java/net/Socket$SocketInputStream` would put the same (class, name,
// descriptor) triples in two registrars, and `net_phase_e::
// register_phase_e_networking` runs AFTER `register_p68_ssl` (lib.rs 18214 vs
// 18191), so last-write-wins would silently hand every TLS stream to the
// plain-socket natives — which resolve their id through `sock_get(owner)
// .stream_id` into the RAW `s2_registry.streams` table, not the TLS one.
pub const TLS_APP_IN_CLASS: &str = "sun/security/ssl/SSLSocketImpl$AppInputStream";

/// See [`TLS_APP_IN_CLASS`].
pub const TLS_APP_OUT_CLASS: &str = "sun/security/ssl/SSLSocketImpl$AppOutputStream";

/// The pre-2026-08-12 carriers. **Still registered, deliberately.**
///
/// `net_phase_e::register_phase_e_networking` mints these two names from its
/// own `java/net/Socket.getInputStream()`/`getOutputStream()` when the socket
/// turns out to be a layered `SSLSocket` (net_phase_e.rs ~5273/5296), and that
/// file is not this lane's to edit. Dropping the registrations here would leave
/// that mint site with a well-formed carrier and no implementation — turning
/// today's `NoClassDefFoundError` at the mint into an `UnsatisfiedLinkError` at
/// the first `read()`, which is exactly the half-change
/// `no_image_receiver.rs::STRICT_STILL_FABRICATES` warns about, running in the
/// other direction.
///
/// `scripts/baselines/jdk-only-gated-never-delete.tsv` also pins all eight rows
/// by name, so deleting them trips a gate as well as a caller.
pub const TLS_LEGACY_IN_CLASS: &str = "javax/net/ssl/SSLSocketInputStream";

/// See [`TLS_LEGACY_IN_CLASS`].
pub const TLS_LEGACY_OUT_CLASS: &str = "javax/net/ssl/SSLSocketOutputStream";

/// Whether `this` is one of the four carriers above.
///
/// Used by `SSLSocket.close()V` to refuse a receiver that is a *stream* — see
/// the guard there for why one can arrive.
pub fn is_tls_stream_carrier(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    match ctx.class_name_of_id(ctx.class_id_of_object(this)) {
        Some(name) => {
            name == TLS_APP_IN_CLASS
                || name == TLS_APP_OUT_CLASS
                || name == TLS_LEGACY_IN_CLASS
                || name == TLS_LEGACY_OUT_CLASS
        }
        None => false,
    }
}

/// Allocate a TLS stream carrier of `class_name` bound to `tls_id`.
///
/// The id is recorded TWICE and both are load-bearing:
///
/// * in the **appended** slot — `try_alloc_with_appended_slots` puts the
///   native's private slot ABOVE every field the real class declares, so on
///   `AppInputStream` (7 declared fields on JDK 25: `oneByte`, `buffer`,
///   `appDataIsAvailable`, `readLock`, `isClosing`, `hasDepleted`, `this$0`)
///   the `Int` lands at slot 7 and not on top of `appDataIsAvailable`. The
///   pre-existing code wrote slot 0 unconditionally, which was harmless only
///   because the fabricated carrier declared nothing; on a real layout it is
///   the W7-49 aliasing shape.
/// * in `net_phase_e`'s identity-keyed side table, which is layout-independent
///   and is what answers for a carrier some other path allocated.
///
/// Readers use [`tls_stream_id`], which reads the LAST slot and falls back to
/// the side table — see there for why the last slot is the right index.
pub fn alloc_tls_stream(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    tls_id: i32,
    owner: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let owner_h = scope.root(owner);
    let (obj, base) = crate::try_alloc_with_appended_slots(&mut *scope, class_name, 1)?;
    let owner = scope.get(&owner_h);
    scope.set_field_by_name(obj, "this$0", Value::Object(Some(owner)));
    let ctx = &mut *scope;
    // Layout-independent record first: if the `set_field` below is ever
    // clamped away, the side table still answers.
    crate::net_phase_e::sock_set_for_create(ctx, obj, 0, tls_id);
    ctx.set_field(obj, base, Value::Int(tls_id));
    Ok(obj)
}

/// The `s2_registry` TLS id a stream carrier is bound to, or `-1`.
///
/// **Why the last slot.** Every carrier this file hands out is allocated by
/// [`alloc_tls_stream`] with `width == 1`, so its width is `base + 1` and the
/// private slot is `object_num_fields(this) - 1` — one header read, no class
/// lookup by name. That matters: `SSLSocketInputStream.read()I` is called ~134
/// million times by `TestSsl.testPost` alone, and the measured budget for the
/// whole native body is ~265 ns. Asking `appended_slot_base_for_class` per call
/// would put a string-keyed class lookup on that path.
///
/// It is only sound BECAUSE the receiver is one this file allocated: on any
/// other instance of the same class (a real JSSE `AppInputStream`, were one
/// ever to reach here) the last slot is `this$0`, a reference, `as_int()`
/// answers `None`, and the side-table fallback runs. That is the
/// W7-49 "a base cannot be recovered from a foreign receiver's width" caveat,
/// satisfied by never trusting the slot's *presence* — only a non-negative
/// `Int` read out of it.
pub fn tls_stream_id(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    let n = ctx.object_num_fields(this);
    if n > 0 {
        if let Some(id) = ctx.get_field(this, n - 1).as_int() {
            if id >= 0 {
                return id;
            }
        }
    }
    crate::net_phase_e::sock_stream_id_for_upcall(ctx, this)
}

/// Stamp a stream carrier's id slot to `-1` so a second `close()` is a no-op.
pub fn tls_stream_clear_id(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let n = ctx.object_num_fields(this);
    if n > 0 {
        ctx.set_field(this, n - 1, Value::Int(-1));
    }
}

// SSLSession field layout: 4 fields.
//   0 = protocol String
//   1 = cipher String
//   2 = tls_id Int (s2_registry id of the backing TLS stream; -1 after close)
//   3 = attribute map (java.util.HashMap or null) — E42, see below
//
// E42 (2026-08-13): **was 3, and the missing 4th slot was a live defect, not a
// missing feature.** `t27_tls`'s JSSE attribute API (`putValue`/`getValue`/
// `removeValue`/`getValueNames`) stores its `java.util.HashMap` in the
// session's LAST slot, and on the 3-field shape the last slot was
// `NEW13_SESS_TLSID` — which is also the slot `t27_tls::session_has_negotiated`
// reads to decide whether anything was negotiated at all. A single `putValue`
// therefore turned `Int(-1)` ("never connected") into a `HashMap` reference,
// which that predicate cannot read as an id, so `isValid()` went back to
// `true` and `getId()` back to 32 fabricated bytes: **an unrelated convenience
// API resurrected the exact fabrication the E12 and E22 lanes removed.**
// `invalidate()` was a second writer of the same slot and inverted outright
// (`Int(-1) -> Int(0)`, a VALID stream id — invalidating the null session made
// it valid). Both were stopped defensively in E31 by making the attribute API
// a NO-OP for shapes with no dedicated slot; this is the structural fix behind
// that, and it is E31-1's NOMINATION 1 and 2.
//
// The writer is not hypothetical: Jetty's `SecureRequestCustomizer
// .retrieveSni()` calls `getValue()` then `putValue()` on EVERY SSL request,
// and this shape is what `new13_resolve_socket_session` hands it.
//
// **Why 4 and not any other width.** Widening a session shape moves it through
// `t27_tls`'s width tables, and those tables are the readers that decide what
// each slot MEANS. At width 4 all four of them already agree with this layout,
// so nothing here retargets an existing slot — the change is purely additive:
//
// | `t27_tls` reader           | at width 3        | at width 4        |
// |----------------------------|-------------------|-------------------|
// | `session_proto_slot`       | 0 (protocol)      | 0 — unchanged     |
// | `session_cipher_slot`      | 1 (cipher)        | 1 — unchanged     |
// | `sslsess_attrs_slot`       | `None`            | **`Some(3)`**     |
// | `session_has_negotiated`   | slot 2 `>= 0`     | see the CAVEAT    |
// | `getCreationTime` (`> 5`)  | `epoch_millis_now`| unchanged         |
//
// Width 4 is also already `SSLServerSocket.accept`'s shape (`t27_tls`, "4-field
// synthetic session: proto, cipher, streamId, attrs") — this layout is
// BYTE-IDENTICAL to it, so the widening retires a distinct width rather than
// adding one. Five `javax/net/ssl/SSLSession` widths in this tree become four.
//
// **CAVEAT — the one co-requisite, and it is BLOCKING.**
// `t27_tls::session_has_negotiated`'s `_ => true` arm (which width 4 falls
// into) is scoped by the premise "the 4- and 6-field shapes are only minted
// after a handshake". This change makes that premise false for width 4, so the
// arm must be merged with the 3-field one — `3 | 4 => slot 2 >= 0` — IN THE
// SAME COMMIT. That merge is a provable no-op for the accept shape, whose slot
// 2 is `RUSTLS_SOCK_ID_BASE + stream_id` and therefore always `>= 0`; without
// it this widening makes the null session valid again, which is the defect
// above with the sign flipped. `the_widened_null_session_is_still_not_
// negotiated` in this file's test module fails loudly if the two ever part
// company, so the co-requisite is enforced by the build and not by this
// comment. See docs/known-issues/jdk-only/E42-1-*.md.
//
// ---------------------------------------------------------------------------
// G44 — **DO NOT WIDEN THIS TO CARRY THE PEER HOST AND PORT.** The obvious fix
// for four measured rows is the wrong one, and the width tables say so.
//
// MEASURED, `RSslLiveSession` on `9ae371468` (`C:/craton/target-rel3`):
//
// ```text
// CK RSslLiveSession client.peerHost                  = null   WANT localhost
// CK RSslLiveSession client.peerPort.isServerPort     = false  WANT true
// CK RSslLiveSession attrs.shadow.peerHost            = null   WANT localhost
// CK RSslLiveSession attrs.shadow.peerPort.isServerPort = false WANT true
// ```
//
// `t27_tls`'s `getPeerHost`/`getPeerPort` read slot 3 and slot 4 on shapes of
// width `>= 6` and fall back to `session_stream_id` -> `s2_tls_session_info`
// otherwise. The HTTPS client session is width 4 and its slot 2 is
// `net_phase_e::HTTPS_CLIENT_SESSION_MARKER`, which is chosen precisely so that
// every socket-registry lookup MISSES — so the fallback cannot answer, by
// design, and the rows are `null`/`-1`.
//
// Widening this constant does not fix them. Enumerated against the four
// `t27_tls` readers that key on width, and against the fact that the NULL
// session (`new13_alloc_null_ssl_session`) is minted from this same constant:
//
// | reader | at 4 (today) | at 6 | at 7 |
// |---|---|---|---|
// | `session_proto_slot` / `session_cipher_slot` | 0 / 1 | **1 / 0 — SWAPPED** | **1 / 0 — SWAPPED** |
// | `sslsess_attrs_slot` | `Some(3)` | **`None`** — the attribute API silently becomes a no-op | `Some(6)` |
// | `session_has_negotiated` | slot 2 `>= 0` | **`_ => true`** — the null session is valid again, which is E42's defect with the sign flipped | slot 2 `!= 0` |
// | `getSessionContext`'s `engine_shape` (`>= 7`) | not engine | not engine | **engine — also requires membership in `negotiated_session_keys`, which only `engine_session_for` writes, so `getSessionContext()` would answer `null`** |
//
// Width 6 costs the attribute family and the null session; width 7 buys the two
// endpoint slots and loses the three `*.sessionContext.isNull` rows that are
// green today (`client`, `attrs.shadow`, `verifier`). Either way the swap in
// row 1 silently reports the protocol as the cipher suite for every minter of
// this shape. Four rows are not worth that, and a private width used by the
// HTTPS minters alone would re-introduce the fifth width E42 retired.
//
// The fix that costs nothing here is a side table keyed on the SESSION OBJECT,
// exactly like `t27_tls::record_client_peer_chain` — which already solves the
// identical problem for the peer certificate chain on this same shape. That
// puts the reader in `t27_tls` (two lines, ahead of the `session_stream_id`
// fallback in each of `getPeerHost`/`getPeerPort`) and the writers in the two
// HTTPS minters. It is NOMINATION N3 of
// `docs/feature-designs/class-resolution-and-caching.md`.
pub const NEW13_SSL_SESS_FIELDS: usize = 4;

pub const NEW13_SESS_PROTO: usize = 0;

pub const NEW13_SESS_CIPHER: usize = 1;

pub const NEW13_SESS_TLSID: usize = 2;

/// The dedicated JSSE attribute-map slot — `t27_tls::sslsess_attrs_slot`
/// answers `Some(3)` for this width. Every minter of this shape writes
/// `Value::Object(None)` here, so the map is allocated lazily by the first
/// `putValue` and nothing else can be mistaken for one. See
/// [`NEW13_SSL_SESS_FIELDS`] for why the slot exists.
pub const NEW13_SESS_ATTRS: usize = 3;

// ---------------------------------------------------------------------------
// The two "nothing was negotiated" sentinels
// ---------------------------------------------------------------------------
//
// These are NOT stand-ins invented here. They are the JDK's own answers, read
// off `sun.security.ssl.SSLSessionImpl.nullSession` on HotSpot 25.0.3+9-LTS on
// this host (`scratchpad/e12/E12SessionContract.java`, arms A and B — a fresh
// unconnected `SSLSocket`, and an `SSLEngine` before `beginHandshake()`):
//
//     getCipherSuite() = SSL_NULL_WITH_NULL_NULL
//     getProtocol()    = NONE
//     getId()          = byte[0]
//     isValid()        = false
//
// Why answering them is honesty and not invention — the distinction this
// file's `--real-jdk` posture turns on. A fabricated stand-in is a value the
// caller cannot tell apart from a real one. These two can ALWAYS be told
// apart, and JSSE guarantees it (`scratchpad/e12/E12HandshakeSession.java`):
//
//     SSL_NULL_WITH_NULL_NULL in getSupportedCipherSuites() = false
//     setEnabledCipherSuites("SSL_NULL_WITH_NULL_NULL")     = IllegalArgumentException
//     "NONE" in getSupportedSSLParameters().getProtocols()  = false
//     TLS_AES_256_GCM_SHA384 in getSupportedCipherSuites()  = TRUE
//     TLS_AES_128_GCM_SHA256 in getSupportedCipherSuites()  = TRUE
//
// The sentinel suite is *unofferable*: JSSE refuses to enable it, so it can
// never be the outcome of a handshake, so returning it can never be mistaken
// for one. The two literals this file used to fall back to are the exact
// opposite — both are in the supported list and both are what a real TLS 1.3
// handshake genuinely produces, so a caller has no way to tell "we negotiated
// AES-256-GCM" from "we negotiated nothing and the VM guessed".
//
// That is why a fabricated cipher name is worse than a merely wrong one:
// security-sensitive code branches on this string. `if (session
// .getCipherSuite().contains("AES_256"))` before sending a secret gets `yes`
// from a session that has never handshaked.
//
// The old fallbacks `"UNKNOWN"`, `"?"` and `"TLS"` are the third wrong answer:
// distinguishable, but not in JSSE's vocabulary, so a caller matching `^TLS_`
// or looking the name up in the IANA registry gets a *fourth* behaviour that
// matches neither the sentinel nor a real suite. (`"TLS"` is additionally
// misleading: it is the standard `SSLContext.getInstance` ALGORITHM name, so
// it reads as a legitimate protocol answer. Measured: `"TLS"` is not in
// `getSupportedSSLParameters().getProtocols()` either.)
pub const JSSE_NULL_CIPHER_SUITE: &str = "SSL_NULL_WITH_NULL_NULL";

/// See [`JSSE_NULL_CIPHER_SUITE`]. `SSLSession.getProtocol()` for a session
/// that has negotiated nothing — measured, not chosen.
pub const JSSE_NULL_PROTOCOL: &str = "NONE";

/// The TLS versions this VM offers, in HotSpot's preference order.
///
/// **E42 — the ORDER is part of the answer.** Measured, HotSpot 25.0.3+9-LTS
/// (`scratchpad/e42/E42EnabledSets.java`), on a fresh `SSLEngine` and on a
/// never-connected `SSLSocket` alike:
///
/// ```text
///   getEnabledProtocols() = [TLSv1.3, TLSv1.2]
/// ```
///
/// most-preferred first. One site in this file spelled the pair the other way
/// round under the comment `// Default: TLSv1.2, TLSv1.3`, and the order is
/// directly observable — `RSslNullSession` asserts the exact string
/// `"[TLSv1.3, TLSv1.2]"`, and a caller that takes element 0 as "the version
/// we would prefer" reads the reversed list as a downgrade.
pub const JSSE_ENABLED_PROTOCOLS: [&str; 2] = ["TLSv1.3", "TLSv1.2"];

/// The suite list this VM offers, as a fresh Java `String[]`.
///
/// **E42 — one list, one spelling.** `t27_tls::SUPPORTED_CIPHER_SUITE_NAMES` is
/// the single source of truth (`SSLSocket.getSupportedCipherSuites` and
/// `.getEnabledCipherSuites` already both read it), but this file also carried
/// THREE inline copies of it — `SSLSocketFactory.getSupportedCipherSuites`
/// (13 entries), `SSLEngine.getSupportedCipherSuites` (13), and
/// `SSLSocketFactory.getDefaultCipherSuites` (7). All three had drifted: the
/// two 13-entry copies predate the `TLS_DHE_RSA_*` pair, so this VM's own
/// factory disclaimed two suites its sockets negotiate.
///
/// The 7-entry copy is the interesting one, because it was not a stale copy —
/// it modelled a narrower "defaults" set, and no such set exists. Measured,
/// HotSpot 25.0.3+9-LTS (`scratchpad/e42/E42FactoryDefaults.java`):
///
/// ```text
///   SSLSocketFactory.getDefaultCipherSuites().length   = 31
///   SSLSocketFactory.getSupportedCipherSuites().length = 31
///   default equals supported (element-wise)            = true
///   socket.getEnabledCipherSuites() equals both        = true
/// ```
///
/// enabled == default == supported, one array, three doors. So the "defaults
/// are a subset" model was invented, and the harm it does is concrete: a caller
/// that intersects its own configured list against `getDefaultCipherSuites()`
/// — which is precisely what that accessor is for — was told this VM cannot do
/// ChaCha20 or any CBC suite.
pub fn jsse_supported_suite_name_array(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let suites = crate::t27_tls::SUPPORTED_CIPHER_SUITE_NAMES;
    // A typed `String[]` (i6-L2), rooted across every `create_string` (each
    // is an allocation and a GC point) by the shared builder. The inline
    // copies this replaced had the unrooted hazard.
    Ok(crate::t27_tls::materialize_java_string_array(ctx, suites))
}

/// Allocate a `fields`-slot synthetic `class` whose slot `slot` holds `value`,
/// the value (when it is a reference) rooted across the allocation.
///
/// gc-common w15-d: the principal, handshake-status and certificate builders
/// in this file allocated their object FIRST and then created the String they
/// stored in it, so the store went through the object's pre-allocation
/// address; the `X509Certificate` accessors held a String read off another
/// object across the allocation instead. Allocating the object last makes it
/// the final GC point.
#[track_caller]
pub fn p68_alloc_with_slot(
    ctx: &mut dyn NativeContext,
    class: &str,
    fields: usize,
    slot: usize,
    value: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let held = match value {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let obj: ObjectRef = try_alloc_concurrent_synthetic(&mut *scope, class, fields)?;
    let value = match held.as_ref() {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => value,
    };
    scope.set_field(obj, slot, value);
    Ok(obj)
}

/// [`p68_alloc_with_slot`] with a fresh String made from `text`.
#[track_caller]
pub fn p68_alloc_with_string_slot(
    ctx: &mut dyn NativeContext,
    class: &str,
    fields: usize,
    slot: usize,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let s = ctx.create_string(text);
    p68_alloc_with_slot(ctx, class, fields, slot, Value::Object(Some(s)))
}

/// [`JSSE_ENABLED_PROTOCOLS`] as a fresh Java `String[]`.
pub fn jsse_enabled_protocol_array(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    // Typed and rooted — see `jsse_supported_suite_name_array`.
    Ok(crate::t27_tls::materialize_java_string_array(
        ctx,
        &JSSE_ENABLED_PROTOCOLS,
    ))
}

/// Allocate the `NEW13_SSL_SESS_FIELDS`-shaped equivalent of JSSE's
/// `SSLSessionImpl.nullSession`: a real, non-null session object that reports
/// "nothing negotiated" in the JDK's own vocabulary.
///
/// **`SSLSocket.getSession()` is contracted never to return null**, and HotSpot
/// honours that even for a socket that was never connected (arm A of the
/// transcript above returns an `SSLSessionImpl`, not null). A native that hands
/// back `Value::Object(None)` there converts HotSpot's `SSL_NULL_WITH_NULL_NULL`
/// into a `NullPointerException` inside the *caller*, at
/// `getSession().getCipherSuite()` — a crash where the oracle has an answer.
///
/// `tls_id` is written to `NEW13_SESS_TLSID` so the accessors' "is there a live
/// stream" test (`isValid`, `getId`) keeps working; pass `-1` when there is no
/// stream, which is the only case this constructor is for.
pub fn new13_alloc_null_ssl_session(
    ctx: &mut dyn NativeContext,
    tls_id: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let session = crate::t27_tls::alloc_session_carrier(ctx, NEW13_SSL_SESS_FIELDS)?;
    // Same pinning discipline as `new13_alloc_ssl_session`: each
    // `create_string` is a GC point, so `session` and the first string both
    // have to survive the second allocation.
    let pin = ctx.pin_native_root(session);
    let proto_str = ctx.create_string(JSSE_NULL_PROTOCOL);
    let proto_pin = ctx.pin_native_root(proto_str);
    let cipher_str = ctx.create_string(JSSE_NULL_CIPHER_SUITE);
    let session = ctx.read_native_pin(pin, session);
    let proto_str = ctx.read_native_pin(proto_pin, proto_str);
    crate::t27_tls::sess_set(
        &*ctx,
        session,
        NEW13_SESS_PROTO,
        Value::Object(Some(proto_str)),
    );
    crate::t27_tls::sess_set(
        &*ctx,
        session,
        NEW13_SESS_CIPHER,
        Value::Object(Some(cipher_str)),
    );
    crate::t27_tls::sess_set(&*ctx, session, NEW13_SESS_TLSID, Value::Int(tls_id));
    // E42: the attribute slot starts EMPTY rather than unwritten. An
    // allocation default is whatever the allocator leaves behind; writing
    // `Object(None)` explicitly is what makes "no attributes have been set"
    // a state this constructor asserts instead of one it inherits. The map
    // itself is allocated lazily by the first `putValue`
    // (`t27_tls::sslsess_attrs_map`).
    crate::t27_tls::sess_set(&*ctx, session, NEW13_SESS_ATTRS, Value::Object(None));
    ctx.unpin_native_roots(pin);
    Ok(session)
}

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
pub fn new13_build_connector(
    extra_root_ders: &[Vec<u8>],
    danger_skip_native_verify: bool,
    max_protocol: Option<native_tls::Protocol>,
    jsse_default_roots: Option<&[Vec<u8>]>,
) -> Result<native_tls::TlsConnector, String> {
    let mut builder = native_tls::TlsConnector::builder();
    builder.min_protocol_version(Some(native_tls::Protocol::Tlsv12));
    // FIX (tls-client-trust-is-openssl-seclevel): the application named its
    // own trust store with `javax.net.ssl.trustStore`, and this connection's
    // `SSLContext` configured nothing of its own — so JSSE's rule applies:
    // that store REPLACES cacerts, it does not add to it. Handing the anchors
    // to the backend (rather than verifying separately afterwards) keeps
    // OpenSSL's path building, name constraints and hostname check exactly as
    // they are; the only thing that changes is which roots it trusts.
    if let Some(roots) = jsse_default_roots {
        builder.disable_built_in_roots(true);
        for der in roots {
            match native_tls::Certificate::from_der(der) {
                Ok(cert) => {
                    builder.add_root_certificate(cert);
                }
                Err(e) => {
                    tracing::debug!(
                        target: "phases_late::tls",
                        "javax.net.ssl.trustStore: skipping unparseable anchor DER: {}",
                        e
                    );
                }
            }
        }
    }
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
/// the context object's identity: [`p68_trust_roots_key`], `(VM, identity
/// hash)` since gc-common w13-a. It was the RAW ADDRESS, which is not stable
/// across a moving collection (a moved context lost its anchors) and was
/// never removed, so a later factory allocated at a dead one's address
/// inherited its anchors -- which the connector ADDS to the platform set.
/// Each row is now swept with its owner (`t27_tls::TlsOwnerKind::P68TrustRoots`).
///
/// gc-common w27-a: keyed [`SslObjKey`] -- `(VM, the object's weak lock key)`
/// -- so two live contexts (or factories) of one VM with one identity hash no
/// longer share anchors: one context's `init` replaced or erased the other's,
/// and a factory could hand its sockets the other context's trust scope. The
/// `P68TrustRoots` weak-owner row is filed under the lock key, so it names one
/// object. A context's lock key is HELD by `t27_tls` (its own tables outlive
/// the context), so its row is dropped by that weak-owner row, not by
/// [`forget_ssl_security_obj_keys`]; a factory's goes either way.
///
/// Populated by `SSLContext.init`
/// from a `TrustManagerFactory`-produced `TrustManager[]` that is bound to an
/// explicit KeyStore (see `tls.rs::register_trust_manager_factory` /
/// `x509_manager::build_trust_manager_state`); consumed by `getSocketFactory`
/// so the returned `SSLSocketFactory` carries the same trust scope into
/// `createSocket`.
#[allow(clippy::type_complexity)]
pub fn p68_ctx_trust_roots_table(
) -> &'static parking_lot::Mutex<std::collections::HashMap<SslObjKey, Vec<Vec<u8>>>> {
    static T: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<SslObjKey, Vec<Vec<u8>>>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// The [`p68_ctx_trust_roots_table`] key of `obj` (a context or a factory),
/// minting it: [`ssl_obj_key`] (gc-common w27-a). `obj` must be current.
pub fn p68_trust_roots_key(ctx: &dyn NativeContext, obj: ObjectRef) -> SslObjKey {
    ssl_obj_key(ctx, obj)
}

/// Record `owner` (CURRENT address) as the object whose death drops row
/// `key` of [`p68_ctx_trust_roots_table`] (gc-common w13-a). Filed under the
/// row's lock key (gc-common w27-a), so the weak-owner row names `owner` alone.
pub fn note_p68_trust_roots_owner(ctx: &dyn NativeContext, key: SslObjKey, owner: ObjectRef) {
    crate::t27_tls::note_tls_weak_owner(
        ctx,
        crate::t27_tls::TlsOwnerKind::P68TrustRoots,
        key.1,
        owner,
    );
}

/// Drop the [`p68_ctx_trust_roots_table`] row filed under lock key `key`: its
/// owner died (`t27_tls`'s `P68TrustRoots` weak-owner sweep). A lock key
/// belongs to one VM, so the VM half need not be matched. Scans: the table
/// holds a row per context / factory with custom anchors.
pub fn forget_p68_trust_roots(key: u64) -> usize {
    let mut table = p68_ctx_trust_roots_table().lock();
    let before = table.len();
    table.retain(|(_, k), _| *k != key);
    before - table.len()
}

/// Extract the keystore-bound trust anchors (DER, restrictive — i.e. NOT
/// merged with system roots) from a `TrustManager[]`, if any element is the
/// real `javax.net.ssl.X509TrustManager` produced by
/// `TrustManagerFactory.getTrustManagers()` (stamped with the bound keystore
/// id via the `cratonvm$x509tm$id` field / slot 0 fallback — same convention
/// `tls.rs::read_trust_manager_id_from_obj` reads). Returns `None` when the
/// array is null/empty or every element resolves to keystore id 0 (the
/// default/system trust store — nothing extra to add).
pub fn p68_extract_trust_manager_roots(
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
pub fn new13_ctx_max_protocol(protocol_name: &str) -> Option<native_tls::Protocol> {
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
pub fn p68_factory_pinned_protocol_name(
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
    new13_ctx_max_protocol(&name).map(|_| name)}

/// The `native_tls` form of `p68_factory_pinned_protocol_name`, for the
/// connector-based (immediate-connect) client path.
pub fn p68_factory_max_protocol(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<native_tls::Protocol> {
    p68_factory_pinned_protocol_name(ctx, args)
        .as_deref()
        .and_then(new13_ctx_max_protocol)
}

/// FIX (es-restclient-https): look up the custom trust anchors (if any)
/// stashed on `args[0]` (the `SSLSocketFactory` `this`) by `getSocketFactory`.
/// Returns an empty Vec when the factory carries no custom scope (the common
/// case — every existing default-trust `createSocket` caller is unaffected).
pub fn p68_factory_trust_roots(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Vec<Vec<u8>>, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(this))) => {
            // gc-common w27-a: a pure lookup; a factory never keyed has no row.
            let direct = match existing_ssl_obj_key(&*ctx, *this) {
                Some(key) => p68_ctx_trust_roots_table()
                    .lock()
                    .get(&key)
                    .cloned()
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            if !direct.is_empty() {
                return Ok(direct);
            }
            if ctx.object_num_fields(*this) > 0 {
                if let Value::Object(Some(sslctx)) = ctx.get_field(*this, 0) {
                    return Ok(crate::t27_tls::context_trust_root_ders(ctx, sslctx)?);
                }
            }
            Ok(Vec::new())
        }
        _ => Ok(Vec::new()),
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
pub fn p68_factory_java_tm_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<u64>, MethodCallFailed> {
    let Some(Value::Object(Some(factory))) = args.first() else {
        return Ok(None);
    };
    if ctx.object_num_fields(*factory) == 0 {
        return Ok(None);
    }
    let Value::Object(Some(sslctx)) = ctx.get_field(*factory, 0) else {
        return Ok(None);
    };
    Ok(crate::t27_tls::ctx_trust_managers_key_if_attached(
        ctx, sslctx,
    )?)
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
pub type PendingSslConnectCtx = (
    Vec<Vec<u8>>,
    Option<u64>,
    Option<native_tls::Protocol>,
    Option<(String, String)>,
);

#[allow(clippy::type_complexity)]
pub fn pending_ssl_socket_connect_ctx_table(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<SslObjKey, PendingSslConnectCtx>> {
    static T: std::sync::OnceLock<
        parking_lot::Mutex<rustc_hash::FxHashMap<SslObjKey, PendingSslConnectCtx>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// The staging key: the socket's [`SslObjKey`] (gc-common w27-a). It was the
/// socket's identity hash with the calling VM folded into the upper half
/// (gc-common w14-b), so two VMs' sockets never shared a row but two LIVE
/// sockets of one VM that drew one hash did: the second `createSocket()`
/// overwrote the first's staged trust scope, and the first `connect()`
/// consumed the second's -- or found none and fell back to the platform
/// roots, widening an explicit truststore.
pub fn pending_ssl_connect_key(ctx: &dyn NativeContext, sock: ObjectRef) -> SslObjKey {
    ssl_obj_key(ctx, sock)
}

/// `sock` must be the socket's CURRENT address: its lock key is minted from
/// it, and the row goes when that key is freed ([`forget_ssl_security_obj_keys`]),
/// so a socket that dies without ever being connected takes its staged trust
/// scope with it instead of keeping it -- and, through `java_tm_key`, a dead
/// `SSLContext`'s `TrustManager` rows -- until VM teardown (gc-common w16-d).
pub fn stash_pending_ssl_socket_connect_ctx(
    ctx: &dyn NativeContext,
    sock: ObjectRef,
    extra_roots: Vec<Vec<u8>>,
    java_tm_key: Option<u64>,
    max_protocol: Option<native_tls::Protocol>,
) {
    if matches!(max_protocol, Some(native_tls::Protocol::Tlsv12)) {
        crate::net_phase_e::sock_set(ctx, sock, |s| {
            s.enabled_protocols = Some(vec!["TLSv1.2".into()])
        });
    }
    let identity = java_tm_key.and_then(crate::t27_tls::socket_identity_for_context_key);
    let key = pending_ssl_connect_key(ctx, sock);
    pending_ssl_socket_connect_ctx_table()
        .lock()
        .insert(key, (extra_roots, java_tm_key, max_protocol, identity));
}

/// Consume the scope staged for `sock` (CURRENT address); the default -- no
/// custom roots, no `TrustManager` key, no ceiling -- when none was staged.
pub fn take_pending_ssl_socket_connect_ctx(
    ctx: &dyn NativeContext,
    sock: ObjectRef,
) -> PendingSslConnectCtx {
    let Some(key) = existing_ssl_obj_key(ctx, sock) else {
        return Default::default();
    };
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
/// Everything `new13_connect_and_handshake` will need, parked between
/// `SSLSocket.connect()` and the first use of the socket.
///
/// See `servlet::PENDING_CONNECT_SOCK_ID_BASE` for why the handshake is not
/// run at connect time.
pub struct PendingConnectSocket {
    pub host: String,
    pub port: u16,
    pub extra_roots: Vec<Vec<u8>>,
    pub java_tm_key: Option<u64>,
    pub max_protocol: Option<native_tls::Protocol>,
    pub client_identity: Option<(String, String)>,
    /// The connection `SSLSocket.connect` ESTABLISHED, parked for the deferred
    /// handshake to run on.
    ///
    /// `connect` used to open a connection, drop it, and let the later
    /// handshake open a fresh one. The peer then sees TWO connections for one
    /// `SSLSocket`: a server that accepts exactly one per client accepts the
    /// first (whose handshake reads EOF, because the client never speaks on
    /// it) and has left `accept()` by the time the second arrives — so that
    /// one waits out the client's 30 s socket timeout and reports
    /// `HandshakeError::WouldBlock`, "the handshake process was interrupted".
    /// Measured end to end against H2's own `NetUtils` in one process: server
    /// failed at 629 ms, client at 30 980 ms.
    ///
    /// `None` only when the established stream could not be handed over, in
    /// which case the handshake reconnects exactly as it did before.
    pub tcp: Option<std::net::TcpStream>,
    /// The VM that parked this socket; its teardown drops the row
    /// (`forget_vm_parked_tls_sockets`).
    pub vm: usize,
    /// The `SSLSocket`'s weak lock key (the second half of its
    /// [`ssl_sock_key`]; gc-common w16-d, w27-a). When the socket dies
    /// without its first I/O or `close()`, the lock-key sweep frees the key
    /// and [`forget_ssl_security_obj_keys`] drops the row and closes `tcp`,
    /// where HotSpot's `Socket` cleaner would close the descriptor.
    pub sock_key: u64,
}

/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0), on a reading of all three
/// acquisition sites: `stash_pending_connect_socket` scans for a free id and
/// inserts, `take_pending_connect_socket` removes, and
/// `drop_pending_connect_socket_if_any` removes. None touches `ctx`, so the
/// guard is never held across a re-entry into the VM — which is the whole of
/// what the level claims. The later sites (VM teardown, the parked-key scan
/// and, since gc-common w16-d, the dead-socket drop -- in
/// [`forget_ssl_security_obj_keys`] since w27-a) keep to the same rule, and
/// drop every removed row -- which closes its `TcpStream` -- after releasing
/// the guard.
pub fn pending_connect_sockets() -> &'static cratonvm_types::lock_order::OrderedPlMutex<
    rustc_hash::FxHashMap<i32, PendingConnectSocket>,
> {
    static T: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<
            rustc_hash::FxHashMap<i32, PendingConnectSocket>,
        >,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            rustc_hash::FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

pub fn stash_pending_connect_socket(entry: PendingConnectSocket) -> i32 {
    let mut pending = pending_connect_sockets().lock();
    let mut id = 1i32;
    while pending.contains_key(&id) {
        id = id.checked_add(1).unwrap_or(1);
    }
    pending.insert(id, entry);
    id
}

/// gc-common w14-b handoff: VM teardown. Drops VM `vm`'s parked
/// connections (closing each `TcpStream` outside the lock) and its staged
/// `createSocket()` trust scopes. Other VMs' rows are left alone.
pub fn forget_vm_parked_tls_sockets(vm: usize) {
    let dropped: Vec<PendingConnectSocket> = {
        let mut pending = pending_connect_sockets().lock();
        let ids: Vec<i32> = pending
            .iter()
            .filter(|(_, p)| p.vm == vm)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| pending.remove(&id))
            .collect()
    };
    drop(dropped);
    pending_ssl_socket_connect_ctx_table()
        .lock()
        .retain(|(owner, _), _| *owner != vm);
}

/// Consume the parked state for `pending_id`. `None` means it was already
/// consumed -- a handshake is a once-only event, so a second attempt is an
/// error, exactly as it is for the layered range.
pub fn take_pending_connect_socket(pending_id: i32) -> Option<PendingConnectSocket> {
    pending_connect_sockets().lock().remove(&pending_id)
}

/// Whether a socket whose handshake is still parked -- staged by the zero-arg
/// `createSocket()` (`pending_ssl_socket_connect_ctx_table`) or connected and
/// waiting for its first I/O (`pending_connect_sockets`) -- will run its
/// post-handshake Java trust check under `SSLContext` key `key`.
///
/// gc-common w13-a: `t27_tls::release_dead_ssl_contexts` keeps a dead
/// context's `TrustManager`s while this answers `true`. The two tables are
/// scanned (both are small, and short-lived per row) and locked one after the
/// other, never together.
pub fn parked_sockets_name_ctx_key(key: u64) -> bool {
    let staged = pending_ssl_socket_connect_ctx_table()
        .lock()
        .values()
        .any(|(_, tm_key, _, _)| *tm_key == Some(key));
    staged
        || pending_connect_sockets()
            .lock()
            .values()
            .any(|p| p.java_tm_key == Some(key))
}

/// `close()` on a socket that was connected but never used: drop the parked
/// state so the entry (and its trust roots) is not retained for the life of
/// the process. Silent no-op for any other id.
pub fn drop_pending_connect_socket_if_any(tls_id: i32) {
    if tls_id >= crate::servlet::PENDING_CONNECT_SOCK_ID_BASE
        && tls_id < crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
    {
        let _ = take_pending_connect_socket(tls_id - crate::servlet::PENDING_CONNECT_SOCK_ID_BASE);
    }
}

/// TCP-connect and KEEP the connection -- `SSLSocket.connect` has to fail HERE
/// for an unreachable peer (JSSE does the real TCP connect at this point), and
/// the deferred TLS handshake then runs on THIS connection.
///
/// It used to drop the stream (`.map(|_| ())`) and let the handshake dial
/// again. Two connections per `SSLSocket` is observable to the peer, and
/// against a one-`accept()`-per-client server it costs the client a full 30 s
/// socket timeout — see `PendingConnectSocket::tcp`.
///
/// H2 `TcpServer.isRunning()` is the caller that makes the connect/handshake
/// split visible in the first place: it needs "the port answers" to be decided
/// by connect(), and it needs "the certificate is untrusted" NOT to be. That
/// still holds — the connection is established here, only the handshake waits.
pub fn new13_tcp_reachability_probe(
    host: &str,
    port: u16,
    timeout_ms: i32,
) -> std::io::Result<std::net::TcpStream> {
    use std::net::{TcpStream, ToSocketAddrs};
    if timeout_ms <= 0 {
        return TcpStream::connect((host, port));
    }
    let mut last = std::io::Error::new(
        std::io::ErrorKind::AddrNotAvailable,
        format!("no address for {host}:{port}"),
    );
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Borrow the parked TCP connection without starting the TLS handshake.
pub fn with_pending_tcp<R>(
    tls_id: i32,
    f: impl FnOnce(&std::net::TcpStream) -> R,
) -> Option<R> {
    let table = pending_connect_sockets().lock();
    table
        .get(&(tls_id - crate::servlet::PENDING_CONNECT_SOCK_ID_BASE))
        .and_then(|p| p.tcp.as_ref())
        .map(f)
}

/// Replay socket options before a deferred connection is published.
pub fn apply_ssl_tcp_options(
    ctx: &dyn NativeContext,
    sock: ObjectRef,
    tcp: &std::net::TcpStream,
) -> std::io::Result<()> {
    let side = crate::net_phase_e::sock_get(ctx, sock);
    tcp.set_read_timeout(if side.read_timeout_ms == 0 {
        None
    } else {
        Some(std::time::Duration::from_millis(
            side.read_timeout_ms as u64,
        ))
    })?;
    tcp.set_nodelay(side.tcp_no_delay)?;
    socket2::SockRef::from(tcp).set_keepalive(side.keep_alive)
}

pub fn new13_ssl_socket_connect(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    release_dead_socket_listener_roots(ctx);
    if new13_resolve_tls_id(ctx, this) >= 0 {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/net/SocketException",
            "already connected",
        ));
    }
    // gc-common w16-d: the socket owns the connection parked below, weakly
    // (`PendingConnectSocket::sock_key`). Keyed here, while `this` is still
    // the native's own argument: nothing before the blocking region below can
    // collect (`read_inet_socket_address` only reads fields), and the lock
    // key is re-addressed by every moving collection after it (w27-a: the
    // socket's weak lock key, no longer `(VM, identity hash)`).
    let sock_key = ssl_sock_key(ctx, this).1;
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
    let (extra_roots, java_tm_key, max_protocol, client_identity) =
        take_pending_ssl_socket_connect_ctx(ctx, this);
    // JSSE: `connect` opens the TCP connection and stops. The handshake runs
    // at the first read/write, at `startHandshake()`, or at `getSession()` --
    // the four points `ensure_layered_handshake_started` is already wired to.
    // Running it here instead made connect() inherit every handshake failure,
    // which is what `TestTools.testSSL` reported as `Expected: 0 actual: 1`:
    // H2's `TcpServer.isRunning()` connects and closes without any I/O, so on
    // HotSpot it answers "the server is up" while here it answered "the
    // certificate is untrusted" -- 60.8 s later. See
    // `servlet::PENDING_CONNECT_SOCK_ID_BASE`.
    let timeout_ms = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    // gc-common w15-d: the socket is written after the park below (a peer's
    // moving collection can run during it) and after the host String's
    // allocation, so it is rooted across both; every write used to go
    // through the address read on entry.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    // Real network I/O: announce the park so a concurrent STW GC knows this
    // thread is in native code (same requirement as the handshake itself --
    // see `new13_connect_and_handshake`'s T19.H1 comment). Interpreter round
    // i3 wave 81, lane L6: JSSE's `connect` blocks in the native `Net.connect`:
    // RUNNABLE in native, as the socket families wave 59 measured.
    scope.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let probe = new13_tcp_reachability_probe(&host, port as u16, timeout_ms);
    scope.end_blocking_region();
    let tcp = match probe {
        Ok(s) => s,
        Err(e) => {
            return Err(RuntimeError::IOException {
                message: format!("Connection refused: {host}:{port}: {e}"),
            }
            .into());
        }
    };
    let sock = scope.get(&this_h);
    apply_ssl_tcp_options(&*scope, sock, &tcp).map_err(|e| RuntimeError::IOException {
        message: e.to_string(),
    })?;
    let local_port = tcp.local_addr().map(|a| i32::from(a.port())).unwrap_or(0);
    let pending_id = stash_pending_connect_socket(PendingConnectSocket {
        host: host.clone(),
        port: port as u16,
        extra_roots,
        java_tm_key,
        max_protocol,
        client_identity,
        tcp: Some(tcp),
        vm: scope.vm_identity(),
        sock_key,
    });
    let pending_tls_id = crate::servlet::PENDING_CONNECT_SOCK_ID_BASE + pending_id;
    // Same field/side-table bookkeeping as `new13_finish_socket`, minus the
    // `SSLSession`: there is no session until a handshake has run, and
    // `getSession()` builds one after driving it (real JSSE does exactly
    // this). The side-table write is the authoritative one -- see
    // `new13_finish_socket`'s comment on the dropped Int field write.
    let host_obj = scope.create_string(&host);
    let sock = scope.get(&this_h);
    sock_set_field(
        &*scope,
        sock,
        NEW13_SOCK_HOST,
        Value::Object(Some(host_obj)),
    );
    crate::net_phase_e::sock_set_for_create_with_local_port(
        &*scope,
        sock,
        port,
        local_port,
        pending_tls_id,
    );
    sock_set_field(&*scope, sock, NEW13_SOCK_PORT, Value::Int(port));
    sock_set_field(&*scope, sock, NEW13_SOCK_TLSID, Value::Int(pending_tls_id));
    sock_set_field(&*scope, sock, NEW13_SOCK_CLOSED, Value::Int(0));
    Ok(None)
}

/// NEW-13: allocate an `SSLSession` synthetic object populated from the
/// session info captured by `s2_tls_connect`.
pub fn new13_alloc_ssl_session(
    ctx: &mut dyn NativeContext,
    tls_id: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let session = crate::t27_tls::alloc_session_carrier(ctx, NEW13_SSL_SESS_FIELDS)?;
    // gc-common w11-a: a new session object starts with no side-table rows a
    // collected session left under its identity key.
    crate::t27_tls::forget_session_rows(ctx, session);
    if let Some(chain) = crate::t27_tls::socket_client_local_chain(tls_id) {
        crate::t27_tls::record_local_cert_chain(ctx, session, chain);
    }

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
            // NEITHER registry knows this id, so nothing was negotiated that
            // this VM can report. Answer JSSE's own "no negotiation" pair
            // rather than a plausible-looking guess — see
            // `JSSE_NULL_CIPHER_SUITE` for why that is honesty and not
            // invention, and why the previous literal was the dangerous kind
            // of wrong: `TLS_AES_128_GCM_SHA256` is in HotSpot's supported
            // list, so a caller could not tell this answer from a real
            // handshake's. The comment 12 lines above already recorded that
            // this fallback had once been reached by a live TLS 1.3
            // connection and had mis-reported its suite.
            None => (
                String::from(JSSE_NULL_PROTOCOL),
                String::from(JSSE_NULL_CIPHER_SUITE),
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
    crate::t27_tls::sess_set(
        &*ctx,
        session,
        NEW13_SESS_PROTO,
        Value::Object(Some(proto_str)),
    );
    crate::t27_tls::sess_set(
        &*ctx,
        session,
        NEW13_SESS_CIPHER,
        Value::Object(Some(cipher_str)),
    );
    crate::t27_tls::sess_set(&*ctx, session, NEW13_SESS_TLSID, Value::Int(tls_id));
    // E42: see `new13_alloc_null_ssl_session` — the attribute slot is
    // explicitly empty, not merely unwritten.
    crate::t27_tls::sess_set(&*ctx, session, NEW13_SESS_ATTRS, Value::Object(None));
    // FIX (netty-https-client-trust residual): record the peer chain this
    // client connection already captured so a later `getPeerCertificates()`
    // on THIS session object doesn't spuriously see "no certificate" — see
    // `t27_tls::record_client_peer_chain` doc comment.
    if let Some(chain) = crate::servlet::s2_tls_peer_cert_chain_der(tls_id) {
        crate::t27_tls::record_client_peer_chain(ctx, session, chain);
    }
    let session = ctx.read_native_pin(pin, session);
    ctx.unpin_native_roots(pin);
    Ok(session)
}

/// Record, on a CLIENT socket's session, the endpoint it dialled (what `getPeerHost()` / `getPeerPort()`
/// answer) and the SNI name the connection carried (`getRequestedServerNames()`): the host and port the
/// socket was created for, from its raw slots (`new13_*` sockets) or its side-table row
/// (`createSocket(String, int)`).
pub fn new13_record_client_session_peer(
    ctx: &mut dyn NativeContext,
    socket: ObjectRef,
    session: ObjectRef,
) {
    let host = match sock_field(&*ctx, socket, NEW13_SOCK_HOST) {
        Value::Object(Some(h)) => ctx.read_string(h).unwrap_or_default(),
        _ => crate::net_phase_e::sock_get(&*ctx, socket).host,
    };
    let port = match sock_field(&*ctx, socket, NEW13_SOCK_PORT) {
        Value::Int(p) if p > 0 => p,
        _ => crate::net_phase_e::sock_get(&*ctx, socket).port,
    };
    crate::t27_tls::record_session_peer_endpoint(&*ctx, session, &host, port);
    let sni = (!host.is_empty() && crate::t27_tls::jsse_would_send_sni(&host)).then_some(host);
    crate::t27_tls::record_session_sni(&*ctx, session, sni, false);
}

/// Turn a failed TLS stream read/write into the exception JSSE raises for it.
///
/// A socket whose handshake never completed reports that at its FIRST I/O,
/// as `javax.net.ssl.SSLHandshakeException` — the accepted-but-unhandshaked
/// case is the server side of a rejected connection, and it reaches here
/// because `SSLServerSocket.accept()` must not throw for it (see
/// `t27_tls::TlsServerStream::HandshakeFailed`). Every other I/O error is an
/// ordinary `IOException`, as before.
pub fn tls_io_failure(ctx: &mut dyn NativeContext, tls_id: i32, e: std::io::Error) -> MethodCallFailed {
    match crate::servlet::s2_tls_handshake_failure(tls_id) {
        Some(reason) => {
            crate::phases_early::throw_jca_exc(ctx, "javax/net/ssl/SSLHandshakeException", &reason)
        }
        None if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) =>
        {
            RuntimeError::SocketTimeoutException {
                message: "Read timed out".into(),
            }
            .into()
        }
        None if e.kind() == std::io::ErrorKind::Interrupted && e.to_string() == "socket closed" => {
            crate::phases_early::throw_jca_exc(ctx, "java/net/SocketException", "Socket closed")
        }
        None => RuntimeError::IOException {
            message: e.to_string(),
        }
        .into(),
    }
}

/// Resolve an `InetAddress` argument without depending on its implementation
/// class.  Real JSSE factories expose all of the `SocketFactory` overloads;
/// our P68 bridge must do the same because its synthetic factory is allocated
/// as `javax/net/ssl/SSLSocketFactory` itself.
pub fn p68_inet_address_host(
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
pub fn p68_create_socket_inet_address(
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
    let extra_roots = p68_factory_trust_roots(ctx, args)?;
    let java_tm_key = p68_factory_java_tm_key(ctx, args)?;
    let max_protocol = p68_factory_max_protocol(ctx, args);
    new13_do_create_socket(
        ctx,
        &host,
        port as u16,
        &extra_roots,
        java_tm_key,
        max_protocol,
    )
}

/// NEW-13: real, blocking TCP-connect + TLS client handshake shared by
/// `new13_do_create_socket` (the immediate-connect `createSocket(host, port)`
/// overloads) and `new13_ssl_socket_connect` (the deferred
/// `createSocket()` + later `Socket.connect(SocketAddress, timeout)` pattern
/// — see that function's doc comment for why the two need the same logic
/// applied to a not-yet-allocated vs. an already-allocated `SSLSocket`).
/// Returns the `s2_registry` TLS stream id on success.
pub fn new13_connect_and_handshake(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: u16,
    extra_root_ders: &[Vec<u8>],
    java_tm_key: Option<u64>,
    max_protocol: Option<native_tls::Protocol>,
) -> Result<i32, MethodCallFailed> {
    new13_connect_and_handshake_on(
        ctx,
        host,
        port,
        extra_root_ders,
        java_tm_key,
        max_protocol,
        None,
    )
}

/// Is the default client path backed by the raw `openssl::SslConnector`
/// (`servlet::s2_openssl_tls_connect_on`) rather than by
/// `native_tls::TlsConnector`?
///
/// Default ON, off with `CRATONVM_TLS_OPENSSL_CLIENT=0`. The switch exists so
/// the two backends can be A/B'd inside ONE binary — a cross-binary comparison
/// against a separately-built tree is not an A/B, and has invented regressions
/// on phases the change cannot reach.
///
/// Unix only, because `openssl` is a Unix-scoped dependency of this crate (see
/// `native-builtins/Cargo.toml`). On Windows the SChannel-backed native-tls
/// path stays exactly as it was, leaf-only chain included: `schannel` exposes
/// no chain accessor through native-tls either, so closing it there needs a
/// different backend, not a different configuration.
#[cfg(unix)]
pub fn openssl_client_enabled() -> bool {
    crate::nbflags().tls_openssl_client
}

/// The Windows half of the same switch, over `servlet::s2_schannel_tls_connect_on`.
///
/// One flag for both, because it selects the same THING on both: whether the
/// default client path runs on a backend that can hand out the peer's chain.
/// The backend differs (OpenSSL / SChannel) because the platform's does; the
/// question does not, and two flags would let a reader think it did.
#[cfg(windows)]
pub fn schannel_client_enabled() -> bool {
    crate::nbflags().tls_openssl_client
}

/// [`new13_connect_and_handshake`], optionally over a connection the caller
/// already established (`SSLSocket.connect`'s deferred-handshake path — see
/// [`PendingConnectSocket::tcp`]). `None` connects here, as before.
#[allow(clippy::too_many_arguments)]
pub fn new13_connect_and_handshake_on(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: u16,
    extra_root_ders: &[Vec<u8>],
    java_tm_key: Option<u64>,
    max_protocol: Option<native_tls::Protocol>,
    established: Option<std::net::TcpStream>,
) -> Result<i32, MethodCallFailed> {
    new13_connect_and_handshake_restricted_on(
        ctx,
        host,
        port,
        extra_root_ders,
        java_tm_key,
        max_protocol,
        established,
        &[],
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn new13_connect_and_handshake_restricted_on(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: u16,
    extra_root_ders: &[Vec<u8>],
    java_tm_key: Option<u64>,
    max_protocol: Option<native_tls::Protocol>,
    established: Option<std::net::TcpStream>,
    protocols: &[String],
    client_identity: Option<&(String, String)>,
) -> Result<i32, MethodCallFailed> {
    #[cfg(unix)]
    let legacy_dsa_context = extra_root_ders.iter().any(|der| {
        openssl::x509::X509::from_der(der)
            .ok()
            .and_then(|cert| cert.public_key().ok())
            .is_some_and(|key| key.dsa().is_ok())
    });
    // JSSE's default trust store, and ONLY for a connection whose own
    // `SSLContext` configured no trust material: an explicit TrustManager or
    // an explicit set of roots is already the answer, and must not be widened
    // by a process-wide property.
    //
    // Deliberately computed AFTER `legacy_dsa_context`, which scans
    // `extra_root_ders` for a DSA key: these anchors are NOT merged into that
    // slice, so a DSA root that happens to sit in the application's trust
    // store cannot switch an unrelated connection onto the legacy OpenSSL
    // path (which runs at security level 0).
    let jsse_default_trust: Option<crate::x509_manager::TrustManagerState> =
        if extra_root_ders.is_empty() && java_tm_key.is_none() {
            match crate::tls::explicit_trust_store_keystore_id(ctx) {
                0 => None,
                id => {
                    let state = crate::x509_manager::build_trust_manager_state(id);
                    (!state.anchor_ders.is_empty()).then_some(state)
                }
            }
        } else {
            None
        };
    let jsse_default_roots: Option<&[Vec<u8>]> = jsse_default_trust
        .as_ref()
        .map(|s| s.anchor_ders.as_slice());
    // Handing OpenSSL the right anchors is necessary and NOT sufficient, and
    // the measurement says so precisely. With the application's own trust
    // store supplied as the connector's roots, `TlsProbe3` still got
    //
    //   SSLHandshakeException: … certificate verify failed … (EE certificate
    //   key too weak)
    //
    // where HotSpot completes the handshake. That is not a trust verdict: it
    // is OpenSSL's SECURITY LEVEL, which at its default of 2 requires a
    // ≥2048-bit RSA key of every peer certificate. The JDK's equivalent knob,
    // `jdk.certpath.disabledAlgorithms`, draws the line at 1024 bits and
    // exempts a trust anchor from the signature-algorithm check entirely — so
    // a 1024-bit MD5-self-signed certificate the application has explicitly
    // installed as its trust anchor is something JSSE accepts and OpenSSL, at
    // this level, cannot be told to.
    //
    // So for THIS case — and only this one — the verdict moves to the
    // validator that already implements the JDK's rules, exactly as the
    // Java-TrustManager path above does: native verification stands down and
    // the captured chain is checked against the configured anchors
    // immediately after connect, failing closed.
    //
    // The blast radius is deliberately the set of connections that are
    // MISCONFIGURED today: an application that named a trust store and was
    // being validated against the platform roots regardless. A connection
    // with no `javax.net.ssl.trustStore` keeps OpenSSL as its verifier,
    // untouched.
    //
    // WHY THIS STAYS, now that the connector below CAN set a security level
    // (`servlet::CLIENT_SECURITY_LEVEL`): the level is a floor on key sizes
    // and signature algorithms, and even level 0 does not make OpenSSL's path
    // builder agree with the JDK's rules — the JDK exempts a TRUST ANCHOR
    // from the signature-algorithm check entirely, which is not a level.
    // MEASURED on the arm this case exists for (`TmProbe`): the VM's own
    // validator accepts a 1024-bit MD5-self-signed certificate installed as
    // the anchor and rejects the same certificate when it is not, matching
    // HotSpot on both. Moving a just-fixed path back onto a different
    // verifier to save one branch would put that verdict at risk for nothing.
    let verify_against_default_roots = jsse_default_roots.is_some();
    // FIX (tls-client-trust-is-openssl-seclevel, residue): JSSE's default
    // trust store for a connection that configured NOTHING — no
    // `javax.net.ssl.trustStore`, no explicit roots, no TrustManager. That is
    // `<java.home>/lib/security/{jssecacerts,cacerts}`, and this path had been
    // using the OS store (`/etc/ssl/certs`, via
    // `SSL_CTX_set_default_verify_paths`) instead.
    //
    // MEASURED (`TrustSetProbe`, keyed on the SHA-256 of each encoded
    // certificate): the OS store is a strict SUPERSET — 122 anchors against
    // cacerts' 118, overlap 118, four trusted here that the JDK does not, one
    // of which is the BUILD HOST'S OWN self-signed machine certificate sitting
    // in `/etc/ssl/certs`. So every default-context client in the VM trusted a
    // CA the JDK deliberately does not, which is the widening direction.
    //
    // `default_trust_store_keystore_id` answers 0 — keep today's platform
    // roots — whenever the file is absent, unreadable, unparseable or parses
    // to zero entries, so a VM with no JDK image (synthetic-jdk mode) is
    // unaffected. Only reachable on the raw connector: `native_tls` has no
    // "replace the built-in roots" that also keeps its own verifier.
    #[cfg(unix)]
    let jdk_default_roots: Option<Vec<Vec<u8>>> = if openssl_client_enabled()
        && !legacy_dsa_context
        && extra_root_ders.is_empty()
        && java_tm_key.is_none()
        && jsse_default_roots.is_none()
    {
        match crate::tls::default_trust_store_keystore_id(ctx) {
            0 => None,
            id => {
                let state = crate::x509_manager::build_trust_manager_state(id);
                (!state.anchor_ders.is_empty()).then_some(state.anchor_ders)
            }
        }
    } else {
        None
    };
    #[cfg(unix)]
    let restriction_roots = jsse_default_roots
        .map(|r| r.to_vec())
        .or_else(|| jdk_default_roots.clone())
        .unwrap_or_else(|| extra_root_ders.to_vec());
    #[cfg(not(unix))]
    let restriction_roots = jsse_default_roots.unwrap_or(extra_root_ders).to_vec();
    // The raw-OpenSSL connector's configuration, or `None` to keep native-tls.
    // Every arm restates what `new13_build_connector` would have configured;
    // the two differ only in what native-tls cannot express (the full peer
    // chain, and the certificate security level).
    #[cfg(unix)]
    let openssl_cfg: Option<crate::servlet::OpensslClientConfig> =
        if openssl_client_enabled() && !legacy_dsa_context {
            // The anchors, and whether they REPLACE the platform set. JSSE's
            // rule is replace for a store that was resolved as "the default
            // trust material" (the property, or cacerts); a per-`SSLContext`
            // custom anchor set has always been ADDITIVE here and stays so.
            let (roots, replace_roots) = match (jsse_default_roots, jdk_default_roots) {
                (Some(roots), _) => (roots.to_vec(), true),
                (None, Some(roots)) => (roots, true),
                (None, None) => (extra_root_ders.to_vec(), false),
            };
            Some(crate::servlet::OpensslClientConfig {
                roots,
                replace_roots,
                skip_verify: java_tm_key.is_some() || verify_against_default_roots,
                max_tls12: matches!(max_protocol, Some(native_tls::Protocol::Tlsv12)),
            })
        } else {
            None
        };
    #[cfg(unix)]
    let use_openssl = openssl_cfg.is_some();
    // The Windows counterpart. Deliberately NARROWER than the Unix one above:
    // it carries the same roots native-tls was already given and makes the
    // same trust decisions, and the only thing that changes is that the peer's
    // chain comes back. `cacerts` is NOT resolved here — see
    // `servlet::SchannelClientConfig` for why "replace the platform roots" is
    // not expressible on SChannel and why faking it would break working
    // connections rather than fix a divergence.
    #[cfg(windows)]
    let schannel_cfg: Option<crate::servlet::SchannelClientConfig> = if schannel_client_enabled() {
        Some(crate::servlet::SchannelClientConfig {
            roots: match jsse_default_roots {
                Some(roots) => roots.to_vec(),
                None => extra_root_ders.to_vec(),
            },
            skip_verify: java_tm_key.is_some() || verify_against_default_roots,
            max_tls12: matches!(max_protocol, Some(native_tls::Protocol::Tlsv12)),
        })
    } else {
        None
    };
    #[cfg(windows)]
    let use_openssl = schannel_cfg.is_some();
    #[cfg(not(any(unix, windows)))]
    let use_openssl = false;
    // Not built at all when the raw connector is in charge: building one reads
    // the OS root store, which is exactly the set this path is moving off.
    let connector = if use_openssl {
        None
    } else {
        Some(
            new13_build_connector(
                extra_root_ders,
                java_tm_key.is_some() || verify_against_default_roots,
                max_protocol,
                jsse_default_roots,
            )
            .map_err(|msg| RuntimeError::IOException { message: msg })?,
        )
    };
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
    // Interpreter round i3 wave 81, lane L6: a TCP connect and a TLS handshake
    // over it block in socket natives on HotSpot (`Net.connect`,
    // `NioSocketImpl.read`'s `read0`): RUNNABLE in native.
    #[cfg(unix)]
    let keep_legacy_tls12 = legacy_dsa_context
        && protocols.iter().any(|p| p == "TLSv1.2")
        && !protocols.iter().any(|p| p == "TLSv1.3");
    #[cfg(not(unix))]
    let keep_legacy_tls12 = false;
    let restricted_cfg =
        if crate::t27_tls::protocol_restriction_is_real(protocols) && !keep_legacy_tls12 {
            Some(
                crate::t27_tls::build_captured_socket_config(
                    &restriction_roots,
                    java_tm_key.is_some() || verify_against_default_roots,
                    protocols,
                    client_identity.map(|(c, k)| (c.as_str(), k.as_str())),
                )
                .map_err(|message| RuntimeError::IOException { message })?,
            )
        } else {
            None
        };
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    // `connector` is `Some` exactly when `openssl_cfg` is `None`, one line
    // apart above. The arms below still spell the impossible combination as a
    // handshake failure rather than an `unwrap`: a panic inside a blocking
    // region is a far worse failure mode than a refused connection.
    let no_connector = || {
        crate::servlet::TlsConnectFailure::Handshake(
            "no TLS connector was configured for this connection".to_string(),
        )
    };
    let connect_result = if let Some(cfg) = restricted_cfg.as_ref() {
        let result = match established {
            Some(tcp) => {
                crate::t27_tls::rustls_client_handshake_over_stream(tcp, cfg.config.clone(), host)
            }
            None => crate::t27_tls::rustls_client_connect(cfg.config.clone(), host, port),
        };
        result
            .map(|id| crate::servlet::RUSTLS_SOCK_ID_BASE + id)
            .map_err(crate::servlet::TlsConnectFailure::Handshake)
    } else {
        #[cfg(unix)]
        let connect_result = match (legacy_dsa_context, openssl_cfg.as_ref(), established) {
            (true, _, Some(tcp)) => {
                crate::servlet::s2_legacy_dsa_tls_connect_on(host, port, extra_root_ders, tcp)
            }
            (true, _, None) => {
                crate::servlet::s2_legacy_dsa_tls_connect(host, port, extra_root_ders)
            }
            (false, Some(cfg), Some(tcp)) => {
                crate::servlet::s2_openssl_tls_connect_on(cfg, host, port, tcp)
            }
            (false, Some(cfg), None) => crate::servlet::s2_openssl_tls_connect(cfg, host, port),
            (false, None, Some(tcp)) => match connector.as_ref() {
                Some(c) => crate::servlet::s2_tls_connect_on(c, host, port, tcp),
                None => Err(no_connector()),
            },
            (false, None, None) => match connector.as_ref() {
                Some(c) => crate::servlet::s2_tls_connect(c, host, port),
                None => Err(no_connector()),
            },
        };
        #[cfg(windows)]
        let connect_result = match (schannel_cfg.as_ref(), established) {
            (Some(cfg), Some(tcp)) => {
                crate::servlet::s2_schannel_tls_connect_on(cfg, host, port, tcp)
            }
            (Some(cfg), None) => crate::servlet::s2_schannel_tls_connect(cfg, host, port),
            (None, Some(tcp)) => match connector.as_ref() {
                Some(c) => crate::servlet::s2_tls_connect_on(c, host, port, tcp),
                None => Err(no_connector()),
            },
            (None, None) => match connector.as_ref() {
                Some(c) => crate::servlet::s2_tls_connect(c, host, port),
                None => Err(no_connector()),
            },
        };
        #[cfg(not(any(unix, windows)))]
        let connect_result = match (connector.as_ref(), established) {
            (Some(c), Some(tcp)) => crate::servlet::s2_tls_connect_on(c, host, port, tcp),
            (Some(c), None) => crate::servlet::s2_tls_connect(c, host, port),
            (None, _) => Err(no_connector()),
        };
        connect_result
    };
    ctx.end_blocking_region();
    // A REJECTED HANDSHAKE is `javax.net.ssl.SSLHandshakeException` on JSSE —
    // callers `catch` that type, and a bare `java.io.IOException` does not
    // match it. Only a failure to reach the peer stays an `IOException`. The
    // message carries the backend's own text (for OpenSSL, the certificate
    // verification error), which is the JSSE analogue of HotSpot's "PKIX path
    // building failed: ... unable to find valid certification path".
    let tls_id = match connect_result {
        Ok(id) => id,
        Err(crate::servlet::TlsConnectFailure::Tcp(e)) => {
            return Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into());
        }
        Err(crate::servlet::TlsConnectFailure::Handshake(msg)) => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                &msg,
            ));
        }
    };
    // gc-common w14-b: the connectors above register the stream with no VM.
    // Claim it for this VM so `servlet::forget_vm_s2_sockets` closes it at
    // teardown (`common-w12d-servlet-os-socket-tables-outlive-their-vm`).
    // One line covers every connector arm; a dead id is ignored.
    crate::servlet::s2_claim_for_vm(ctx.vm_identity(), tls_id);
    if let Some(config) = restricted_cfg.as_ref() {
        crate::t27_tls::record_socket_client_local_chain(tls_id, config.presented_chain());
    }

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
            !legacy_dsa_context || restricted_cfg.is_some()
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

    // The `javax.net.ssl.trustStore` arm of the same fail-closed rule: native
    // verification was stood down for it above, so these anchors are now the
    // ONLY verifier and a missing or unvalidatable chain must abort the
    // socket. `validate_chain` is the same RFC 5280 validator the Java
    // TrustManager shim runs, and the same one that answered exactly as
    // HotSpot did on both arms of `TmProbe` — including accepting a
    // self-signed certificate installed as the anchor itself.
    if let Some(trust) = jsse_default_trust.as_ref() {
        let chain = crate::servlet::s2_tls_peer_cert_chain_der(tls_id).unwrap_or_default();
        if chain.is_empty() {
            let _ = crate::servlet::s2_tls_close(tls_id);
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                "no peer certificate available for javax.net.ssl.trustStore verification",
            ));
        }
        if let Err(e) = crate::x509_manager::validate_chain(&chain, trust) {
            let _ = crate::servlet::s2_tls_close(tls_id);
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                &format!("PKIX path validation failed: {e}"),
            ));
        }
    }
    Ok(tls_id)
}

/// NEW-13: common body for the `SSLSocketFactory.createSocket` overloads that
/// connect immediately. Connects, then allocates a fresh `SSLSocket` and
/// populates it — see `new13_finish_socket` (shared with the deferred
/// `createSocket()` + `connect()` pattern, which populates an
/// *already-allocated* socket instead).
pub fn new13_do_create_socket(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: u16,
    extra_root_ders: &[Vec<u8>],
    java_tm_key: Option<u64>,
    max_protocol: Option<native_tls::Protocol>,
) -> MethodCallResult {
    let tls_id =
        new13_connect_and_handshake(ctx, host, port, extra_root_ders, java_tm_key, max_protocol)?;
    // gcd d1/e: plus the handshake-listener slot (see `alloc_ssl_socket`).
    let sock = alloc_ssl_socket(ctx, NEW13_SSL_SOCK_FIELDS)?;
    let sock = new13_finish_socket(ctx, sock, host, port, tls_id);
    if crate::nbflags().dbg_tls_sock {
        eprintln!(
            "[dbg-tls-sock] thread={:?} new13_do_create_socket built sock={:?} tls_id={}",
            std::thread::current().id(),
            sock,
            tls_id
        );
    }
    Ok(Some(Value::Object(Some(sock?))))
}

/// Populate a (possibly pre-existing) `javax/net/ssl/SSLSocket` object's
/// fields/side-table state from a completed TLS connection. Shared by
/// `new13_do_create_socket` (allocates `sock` fresh, immediately before
/// calling this) and `new13_ssl_socket_connect` (reuses the `SSLSocket`
/// object `SSLSocketFactory.createSocket()` already returned to Java, which
/// is why this takes `sock` rather than allocating one itself). Returns the
/// (possibly GC-forwarded) object reference to use afterward.
pub fn new13_finish_socket(
    ctx: &mut dyn NativeContext,
    sock: ObjectRef,
    host: &str,
    port: u16,
    tls_id: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let pin_base = ctx.pin_native_root(sock);
    let host_obj = ctx.create_string(host);
    let sock = ctx.read_native_pin(pin_base, sock);
    sock_set_field(&*ctx, sock, NEW13_SOCK_HOST, Value::Object(Some(host_obj)));
    sock_set_field(&*ctx, sock, NEW13_SOCK_PORT, Value::Int(port as i32));
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
    let timeout = crate::net_phase_e::sock_get(ctx, sock).read_timeout_ms;
    if let Some(result) = crate::net_phase_e::re1_with_raw_stream(tls_id, |tcp| {
        tcp.set_read_timeout(if timeout == 0 {
            None
        } else {
            Some(std::time::Duration::from_millis(timeout as u64))
        })
    }) {
        if let Err(e) = result {
            ctx.unpin_native_roots(pin_base);
            return Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into());        }    }
    let local_port = crate::net_phase_e::re1_with_raw_stream(tls_id, |tcp| {
        tcp.local_addr().map(|a| i32::from(a.port())).unwrap_or(0)
    })
    .unwrap_or(0);
    crate::net_phase_e::sock_set_for_create_with_local_port(
        ctx,
        sock,
        port as i32,
        local_port,
        tls_id,
    );
    sock_set_field(&*ctx, sock, NEW13_SOCK_TLSID, Value::Int(tls_id));
    sock_set_field(&*ctx, sock, NEW13_SOCK_CLOSED, Value::Int(0));
    // gc-common w15-d: a failed session allocation used to return past the
    // unpin below, leaving the socket rooted for the rest of the frame.
    let session = match new13_alloc_ssl_session(ctx, tls_id) {
        Ok(session) => session,
        Err(e) => {
            ctx.unpin_native_roots(pin_base);
            return Err(e);
        }
    };
    let sock = ctx.read_native_pin(pin_base, sock);
    sock_set_field(
        &*ctx,
        sock,
        NEW13_SOCK_SESSION,
        Value::Object(Some(session)),
    );
    new13_record_client_session_peer(ctx, sock, session);
    let sock = ctx.read_native_pin(pin_base, sock);
    ctx.unpin_native_roots(pin_base);
    Ok(sock)
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
/// `tls-ocsp-clientcert-validation-not-enforced-FIXED.md`,
/// "Residual #2 implementation" for the full trace that found this).
///
/// Keyed [`FactoryIdentity`] -- `(vm_identity, identity_hash)` -- since
/// gc-common w10-e, like its two siblings below: identity hashes collide
/// across VMs by construction, so a bare-hash key let VM B's factory resolve
/// VM A's keystore, key-manager or trust-manager id (and, through
/// `kmf_live_km_id_by_identity`, `PrivateKey` objects in A's heap).
///
/// gc-common w27-a: [`FactoryIdentity`] is `(VM, the factory's weak lock
/// key)` now ([`SslObjKey`]), and the rows go when the lock-key sweep frees
/// the key ([`forget_ssl_security_obj_keys`]). Two LIVE factories of one VM
/// with one identity hash shared all four rows, so one factory's
/// `getKeyManagers()` served the other's keystore -- the wrong client
/// certificate -- and one's re-`init` erased the other's rows.
#[allow(clippy::type_complexity)]
pub fn kmf_keystore_id_by_identity(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<FactoryIdentity, i32>> {
    static T: std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<FactoryIdentity, i32>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// The row key of a `KeyManagerFactory` / `TrustManagerFactory` object:
/// `(vm_identity, weak lock key)` ([`SslObjKey`]; it was `(vm_identity,
/// identity_hash)` until gc-common w27-a). The VM half keeps the per-VM scans
/// (`factory_rows_name_manager_id`, `keystore`'s store-release check) and VM
/// teardown a plain filter.
pub type FactoryIdentity = SslObjKey;

/// Drop VM `vm`'s rows of the three factory identity tables (VM teardown,
/// from `t27_tls::forget_vm_tls_state`; gc-common w10-e).
pub fn forget_vm_factory_identity_rows(vm: usize) {
    kmf_keystore_id_by_identity()
        .lock()
        .retain(|(owner, _), _| *owner != vm);
    kmf_key_manager_cache()
        .lock()
        .retain(|(owner, _), _| *owner != vm);
    kmf_live_km_id_by_identity()
        .lock()
        .retain(|(owner, _), _| *owner != vm);
    tmf_tm_id_by_identity()
        .lock()
        .retain(|(owner, _), _| *owner != vm);
    // gc-common w13-a / w27-a: keyed `(VM, lock key)`, so a VM's rows are
    // recoverable -- and dropped with it.
    p68_ctx_trust_roots_table()
        .lock()
        .retain(|(owner, _), _| *owner != vm);
    // gc-common w15-d: the identity tables that were keyed by the bare hash,
    // and so could not be told apart per VM at all.
    forget_vm_identity_keyed_rows(vm);
}

/// VM teardown for the per-object tables gc-common w15-d re-keyed per VM (and
/// w27-a by [`SslObjKey`]): the `Mac` states (key material, scrubbed on
/// drop), the socket client-auth / endpoint-algorithm rows, the
/// handshake-listener rows and the queued roots of dead sockets' listeners
/// (their global roots die with the VM), the layered-socket owner marks and
/// the factory-initialised set.
///
/// Runs from `t27_tls::forget_vm_tls_state`, BEFORE `forget_vm_lock_keys`
/// frees the VM's keys: the rows are gone by the time
/// [`forget_ssl_security_obj_keys`] sees those keys, so it queues no listener
/// root for a VM that no longer exists and offers no manager id.
pub fn forget_vm_identity_keyed_rows(vm: usize) {
    let owned = |k: &SslObjKey| k.0 == vm;
    // Removed rows are dropped outside the lock: `MacState::drop` zeroes
    // their key bytes.
    let dropped_macs: Vec<MacState> = {
        let mut t = mac_state_table().lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<SslObjKey> = t.keys().copied().filter(|k| owned(k)).collect();
        keys.into_iter().filter_map(|k| t.remove(&k)).collect()
    };
    drop(dropped_macs);
    ssl_sock_auth_state().lock().retain(|k, _| !owned(k));
    ssl_sock_endpoint_alg().lock().retain(|k, _| !owned(k));
    handshake_listeners().lock().retain(|k, _| !owned(k));
    ssl_layered_socket_owners().lock().retain(|k| !owned(k));
    // gc-common w16-d: the listener roots of the VM's dead sockets that no
    // native released yet; like the rows above, they die with the VM.
    dead_socket_listener_roots().lock().remove(&vm);
    jsse_factory_initialized()
        .lock()
        .retain(|(owner, _)| *owner != vm);
}

/// The `SSLSocket`s that own a pending LAYERED handshake
/// (`t27_tls::set_pending_layered_socket_owner`), by [`SslObjKey`]
/// (gc-common w27-a). [`forget_ssl_security_obj_keys`] asks `t27_tls` to drop
/// the pending entry only for a freed key found here -- that call scans
/// `t27_tls`'s pending table, and a sweep frees keys by the thousand. A mark
/// outlives the handshake (it is small, and goes with the socket or the VM).
/// `LockLevel::Scratch`: one set operation per acquisition, nothing nested.
pub fn ssl_layered_socket_owners(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashSet<SslObjKey>> {
    static T: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashSet<SslObjKey>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            rustc_hash::FxHashSet::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Whether one of VM `vm`'s factory rows still names manager state `id` (a
/// km id when `is_km`, else a tm id): the factory can still hand it out, so
/// `t27_tls::release_manager_state_candidates` keeps it. Scans; the tables
/// hold one row per live factory.
pub fn factory_rows_name_manager_id(vm: usize, is_km: bool, id: i32) -> bool {
    if is_km {
        let cached = kmf_key_manager_cache()
            .lock()
            .iter()
            .any(|((owner, _), (_, km))| *owner == vm && *km == id);
        cached
            || kmf_live_km_id_by_identity()
                .lock()
                .iter()
                .any(|((owner, _), km)| *owner == vm && *km == id)
    } else {
        tmf_tm_id_by_identity()
            .lock()
            .iter()
            .any(|((owner, _), tm)| *owner == vm && *tm == id)
    }
}

/// A factory row that named `old` now names `new` (or nothing): offer `old`.
pub fn offer_replaced_manager_id(vm: usize, is_km: bool, old: Option<i32>, new: Option<i32>) {
    if let Some(old) = old.filter(|o| Some(*o) != new) {
        let kind = if is_km {
            crate::t27_tls::TlsOwnerKind::KeyManagerState
        } else {
            crate::t27_tls::TlsOwnerKind::TrustManagerState
        };
        crate::t27_tls::offer_manager_state(vm, kind, old);
    }
}

pub type FactoryIdTable =
    cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashMap<FactoryIdentity, i32>>;

/// The `x509_manager::km_registry` id `getKeyManagers()` answered for a
/// keystore-backed factory, with the keystore id it was built from (gc-common
/// w11-a, `common-w10e-tls-manager-and-keystore-registries-grow-for-the-life-of-a-vm`).
///
/// Every CALL used to build a fresh `KeyManagerState` -- a copy of every
/// alias's chain and private-key DER -- under a new id that nothing ever
/// removes, so a caller that asks the factory again (Tomcat's `SSLUtilBase`
/// does per connector reload, netty per `SslContext`) grew the registry by a
/// keystore copy each time. The JDK's `KeyManagerFactoryImpl` builds ONE
/// `KeyManager` in `engineInit` and hands it back from every
/// `engineGetKeyManagers`, so sharing one state per `init` is also the closer
/// answer. `init(KeyStore, char[])` drops the row, so a re-`init` rebuilds.
/// `LockLevel::Scratch` (L0): every site is one statement over a key built
/// beforehand.
#[allow(clippy::type_complexity)]
pub fn kmf_key_manager_cache() -> &'static cratonvm_types::lock_order::OrderedPlMutex<
    rustc_hash::FxHashMap<FactoryIdentity, (i32, i32)>,
> {
    static T: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<
            rustc_hash::FxHashMap<FactoryIdentity, (i32, i32)>,
        >,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            rustc_hash::FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Sibling of `kmf_keystore_id_by_identity` for the case where the `KeyStore`
/// handed to `init` is NOT one of this VM's own: the `KeyManagerState` was
/// built by enumerating that store live, so what is recorded here is an
/// `x509_manager::km_registry` id, not a keystore id.
///
/// Deliberately a SEPARATE map rather than a sentinel value in the one above:
/// two independent id spaces sharing one integer slot is the exact shape that
/// produced the `KEY_VALUES_MISMATCH` family this page's section A was about —
/// right only while the two counters happened to be aligned.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — two sites: a one-statement
/// `insert` at `init` time, and the `getKeyManagers` read, now bound in a block
/// so its guard drops before the arm that walks the object through `ctx`.
pub fn kmf_live_km_id_by_identity() -> &'static FactoryIdTable {
    static T: std::sync::OnceLock<FactoryIdTable> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            rustc_hash::FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
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
#[allow(clippy::type_complexity)]
pub fn tmf_tm_id_by_identity(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<FactoryIdentity, i32>> {
    static T: std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<FactoryIdentity, i32>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Is `this` the synthetic default factory THIS module's `getInstance` built,
/// rather than a caller's own `TrustManagerFactory`/`KeyManagerFactory`
/// subclass?
///
/// Every native on these two classes is dispatched by the interpreter for
/// SUBCLASS receivers too, and the synthetic layout these handlers assume
/// (`provider`, `factorySpi`, `algorithm` — the real JDK field order) belongs
/// to an object a real subclass never went through. Writing a slot on such a
/// receiver overwrites a field the real constructor already filled.
///
/// Measured 2026-08-13 (netty `handler.ssl` batch 10): `TrustManagerFactory.
/// init(KeyStore)` did `set_field(this, 1, keystore)` unconditionally, and
/// field 1 of the REAL class is `factorySpi`. netty's
/// `SimpleTrustManagerFactory` (the base of `InsecureTrustManagerFactory` and
/// of every per-test factory in this suite) is a real subclass constructed
/// with a real SPI, so `SslContext.buildTrustManagerFactory`'s `tmf.init(ks)`
/// replaced its SPI with the `KeyStore`, and the very next
/// `getTrustManagers()` — which correctly delegates to the real bytecode for a
/// subclass — died on
/// `NoSuchMethodError: java.security.KeyStore.engineGetTrustManagers()`.
/// `init((KeyStore) null)` was the same defect with a null: it nulled
/// `factorySpi` and the real `getTrustManagers()` then NPE'd.
///
/// The runtime class is the whole test: `TrustManagerFactory`'s and
/// `KeyManagerFactory`'s constructors are `protected`, so the only way to hold
/// an instance whose class is EXACTLY the base class is to have got it from
/// `getInstance` — i.e. from the handler right here.
pub fn jsse_factory_is_ours(ctx: &mut dyn NativeContext, this: ObjectRef, base: &str) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(this))
        .is_some_and(|n| n == base)
}

/// The synthetic `TrustManagerFactory` / `KeyManagerFactory` their
/// `getInstance` natives hand out, in the REAL field order (`provider`,
/// `factorySpi`, `algorithm`).
///
/// gc-common w15-d: both natives allocated the factory FIRST and then built
/// the provider object -- `resolve_or_make_provider` allocates and can run
/// Java -- so the three stores went through the factory's pre-allocation
/// address, and the `algorithm` argument was stored through its pre-call
/// address too. The provider is built first here, the algorithm and the
/// provider are rooted, and the factory is the last allocation.
#[track_caller]
pub fn jsse_factory_alloc(
    ctx: &mut dyn NativeContext,
    class: &str,
    provider_name: &str,
    algorithm: Value,
) -> MethodCallResult {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let algorithm_h = match algorithm {
        Value::Object(Some(a)) => Some(scope.root(a)),
        _ => None,
    };
    let provider_obj =
        crate::jca::provider_chain::resolve_or_make_provider(&mut *scope, provider_name)?;
    let provider_h = scope.root(provider_obj);
    let obj: ObjectRef = try_alloc_concurrent_synthetic(&mut *scope, class, 3)?;
    let provider = scope.get(&provider_h);
    let algorithm = match algorithm_h.as_ref() {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => algorithm,
    };
    scope.set_field(obj, 0, Value::Object(Some(provider)));
    scope.set_field(obj, 1, Value::Object(None)); // factorySpi — unused by this stub
    scope.set_field(obj, 2, algorithm);
    Ok(Some(Value::Object(Some(obj))))
}

/// Build and throw a real `java.security.NoSuchAlgorithmException` carrying
/// `msg`. Used by `KeyManagerFactory.getInstance`/`TrustManagerFactory.
/// getInstance` (below) to honour the JCA `getInstance` contract — real JDK
/// rejects an algorithm name no registered provider supports rather than
/// silently substituting a default implementation. Falls back to a catchable
/// `SecurityException` if the real exception class can't be constructed,
/// mirroring the `throw_jca`/`p68_signature_failure` pattern used elsewhere
/// in this crate for the same reason (never swallow a validation failure).
pub fn kmf_tmf_no_such_algorithm(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/security/NoSuchAlgorithmException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
        || {
            RuntimeError::SecurityException {
                message: msg.to_string(),
            }
            .into()
        },
    )
}

/// Per-`javax/net/ssl/SSLSocket` client-authentication and role state, keyed by
/// the socket's [`SslObjKey`] (stable across a moving collection, and the same
/// keying `kmf_keystore_id_by_identity` above uses).
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
///
/// gc-common w15-d: keyed [`ssl_sock_key`] -- `(VM, identity hash)` -- where it
/// was the bare hash, so VM B's socket read the client-auth request VM A set
/// on the socket that drew the same hash. A row now goes with its socket
/// (since w27-a: [`forget_ssl_security_obj_keys`]) and with its VM
/// ([`forget_vm_factory_identity_rows`]); it used to stay for the life of the
/// process.
///
/// gc-common w27-a: keyed [`SslObjKey`], so two live sockets of one VM that
/// drew one hash no longer read and write each other's client-auth request
/// (a server socket's `setNeedClientAuth(true)` reported on another socket).
#[allow(clippy::type_complexity)]
pub fn ssl_sock_auth_state(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<SslObjKey, (i32, i32, i32)>> {
    static T: std::sync::OnceLock<
        parking_lot::Mutex<rustc_hash::FxHashMap<SslObjKey, (i32, i32, i32)>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// The key of an `SSLSocket`'s rows in [`ssl_sock_auth_state`],
/// [`ssl_sock_endpoint_alg`], [`handshake_listeners`], the staged
/// `createSocket()` scope and the parked connections: its [`SslObjKey`],
/// minted (gc-common w27-a; `(calling VM, identity hash)` since w15-d). Every
/// writer calls this with the socket's CURRENT address, before the table's
/// guard; readers call [`existing_ssl_obj_key`].
pub fn ssl_sock_key(ctx: &dyn NativeContext, sock: ObjectRef) -> SslObjKey {
    ssl_obj_key(ctx, sock)
}

pub fn ssl_sock_auth_get(ctx: &dyn NativeContext, this: ObjectRef) -> (i32, i32, i32) {
    let Some(key) = existing_ssl_obj_key(ctx, this) else {
        return (0, 0, 0);
    };
    ssl_sock_auth_state()
        .lock()
        .get(&key)
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
/// socket's [`SslObjKey`] (same keying `ssl_sock_auth_state` above uses).
///
/// The value is a list of `(listener key, global-root handle)`. The
/// handle — not a raw `ObjectRef` — is what makes this safe: these references
/// are stored by one native call and consumed by a later one, across which a
/// moving collection can relocate the listener. `add_global_root` keeps the
/// object reachable and remaps it; `resolve_global_root` hands back the
/// current address. The listener's key rides along so
/// `removeHandshakeCompletedListener` can find the right entry without
/// resolving every root.
///
/// gc-common w27-a: the listener key is the listener's weak lock key (it was
/// its identity hash), so a second listener that drew the first's hash is
/// registered rather than taken for a duplicate, and removing one never
/// removes -- and un-roots -- the other. The global root keeps the listener
/// alive, so its key slot lives as long as the entry. The socket half is
/// keyed [`ssl_sock_key`] ([`SslObjKey`]) and dropped with the socket's lock
/// key ([`forget_ssl_security_obj_keys`]).
///
/// gc-common w15-d: keyed [`ssl_sock_key`]. The key was the bare socket hash,
/// and a global-root HANDLE is only meaningful in the VM that minted it, so a
/// socket of VM B that drew the same hash as a socket of VM A resolved A's
/// handles through B's root table and invoked `handshakeCompleted` on
/// whatever B had filed under those numbers. VM teardown drops the VM's rows
/// ([`forget_vm_factory_identity_rows`]); their roots die with the VM.
///
/// gc-common w16-d: a socket that dies without `close()` loses its row (since
/// w27-a in [`forget_ssl_security_obj_keys`], when its lock key is freed) and
/// its listeners' roots are released by the VM's next socket native
/// ([`release_dead_socket_listener_roots`]). A listener that itself holds
/// its socket still keeps the socket alive through its global root -- see
/// `docs/feature-designs/native-dispatch-architecture.md`.
#[allow(clippy::type_complexity)]
pub fn handshake_listeners(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<SslObjKey, Vec<(u64, usize)>>> {
    static T: std::sync::OnceLock<
        parking_lot::Mutex<rustc_hash::FxHashMap<SslObjKey, Vec<(u64, usize)>>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

