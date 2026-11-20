// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! WP5.2 — `KeyStore` real PKCS#12 + JKS parse.
//!
//! Implements the `engine*` surface real-JDK exposes from
//! `sun/security/pkcs12/PKCS12KeyStore` and `sun/security/provider/JavaKeyStore`
//! (plus the `$JKS` and `$DualFormatJKS` inner-class aliases). Loads a
//! PKCS#12 PFX or a JKS keystore, decrypts shrouded key bags with the supplied
//! password, and exposes private keys + cert chains to the rest of the runtime
//! (TLS, X509KeyManager, X509TrustManager).
//!
//! ## Format detection
//!
//! - PKCS#12 always starts with ASN.1 `SEQUENCE` (`0x30 ..`). Delegates to the
//!   `p12` crate, which gives us `PFX::parse(bytes)` + `PFX::bags(password)`
//!   returning typed `SafeBag`s (cert vs shrouded-key vs other).
//! - JKS starts with the magic `0xFEEDFEED` (4 bytes BE). Hand-rolled walker
//!   (the format is fully open — see Wikipedia: JKS). Trailing 20-byte
//!   integrity tag is verified with the JKS-specific construction:
//!   `SHA1(password_utf16be || "Mighty Aphrodite" || body)` — note this is
//!   *not* a standard HMAC, and an empty password is encoded as the empty
//!   byte sequence (not `[0x00, 0x00]`).
//!
//! ## What we expose to the VM
//!
//! Each loaded store is assigned a 32-bit `store_id` and stashed in the
//! per-process `KEYSTORE_REGISTRY`. A synthetic Java mirror is allocated for
//! each entry the VM asks about:
//!
//! - `engineGetKey` returns a `java/security/PrivateKey` proxy whose 4 fields
//!   are `(algo_idx, key_size_bits, key_len_bytes, key_id)`. The TLS layer
//!   pulls the DER through `keystore_get_private_key(store_id, alias)` /
//!   `keystore_get_chain` (the public shim exported below).
//! - `engineGetCertificate` returns a `java/security/cert/X509Certificate`
//!   proxy with fields `(subject, issuer, cert_id)`. The X.509 parser at
//!   `crate::security_manager::x509` is *not* called from this module — it
//!   would create a back-edge with that crate. Instead we keep the DER and
//!   let consumers decode on demand.
//!
//! ## Tests
//!
//! Embedded `#[cfg(test)] mod tests` covers:
//! 1. Round-trip JKS load + alias enumeration on a known fixture.
//! 2. Round-trip PKCS#12 load + alias enumeration on a known fixture.
//! 3. Magic-byte format detection.
//! 4. JKS HMAC pass / mismatch on a corrupted byte.
//! 5. Wrong-password rejection on a shrouded PKCS#12 key bag.
//! 6. End-to-end `engineLoad` -> `engineAliases` -> `engineGetCertificate`
//!    on the JKS fixture through a `MockNativeContext`.
//!
//! Fixture bytes are inlined as `static [u8]` blobs (≤ 4 KiB each) so the
//! tests run hermetically with zero filesystem dependency.

#![allow(clippy::needless_range_loop)]

use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::OnceLock;

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use parking_lot::RwLock;

use crate::crypto_impl;
use crate::try_alloc_concurrent_synthetic;

// ---------------------------------------------------------------------------
// Public model
// ---------------------------------------------------------------------------

/// A single entry parsed from a keystore.
#[derive(Clone, Debug)]
pub struct KeyStoreEntry {
    /// Original alias as it appeared in the file. JKS lowercases at write
    /// time, PKCS#12 preserves whatever the producer wrote.
    pub alias: String,
    /// Creation time in milliseconds since epoch (JKS provides this; PKCS#12
    /// generally does not, in which case we report 0).
    pub creation_time_ms: i64,
    pub kind: EntryKind,
}

/// What flavor of entry this is. Keys are kept as raw DER (PKCS#8 `PrivateKey`
/// SEQUENCE for keys decrypted out of a shrouded bag, or whatever JKS stored
/// — JKS's "encrypted" format is a known-broken SHA-1 stream cipher, but the
/// decryption is still defined and we honour it).
#[derive(Clone, Debug)]
pub enum EntryKind {
    PrivateKey {
        /// Decrypted PKCS#8 `PrivateKey` DER (the bytes of the SEQUENCE,
        /// algorithm + private-key OCTET STRING).
        key_der: Vec<u8>,
        /// Cert chain in DER form, leaf first.
        chain: Vec<Vec<u8>>,
        /// The envelope this entry's OWN password opens, in the protection
        /// format its store type writes, for an entry set through
        /// `setKeyEntry` / `setEntry` with a password.
        ///
        /// KS-5: with nowhere to keep it, an entry password did nothing at
        /// all. `engineStore` had nothing to encrypt with but the STORE
        /// password, and `engineGetKey` had nothing to check a password
        /// against -- which is both halves of the only job an entry password
        /// has.
        ///
        /// `None` for an entry read off disk. There the envelope, if the load
        /// password did not open it, IS `key_der` -- see `load_pkcs12_ex`'s
        /// placeholder arm -- and `keystore_unlock_private_keys` re-attempts
        /// it with whatever password `getKey` was given.
        protected: Option<Vec<u8>>,
    },
    TrustedCert {
        /// X.509 cert DER.
        cert_der: Vec<u8>,
    },
    /// Symmetric key material from a PKCS#12 SecretBag, carried together with
    /// the JCA algorithm name it was stored under.
    ///
    /// The name is not decoration. `SunPKCS12` encodes a `SecretKeyEntry` as
    /// `SEQUENCE { INTEGER 0, AlgorithmId.get(key.getAlgorithm()), OCTET
    /// STRING key.getEncoded() }`, so the algorithm survives a round trip only
    /// as that OID. This module used to discard it and hand every recovered
    /// secret key back as algorithm `"RAW"` -- which is the key's FORMAT, not
    /// its algorithm -- so `Cipher.getInstance(key.getAlgorithm())` on a key
    /// read out of a keystore failed where HotSpot succeeded.
    SecretKey {
        key_bytes: Vec<u8>,
        algorithm: String,
    },
}

/// One loaded keystore. The map keys are case-preserved aliases.
///
/// `IndexMap`, NOT `HashMap` — real JDK's `JavaKeyStore`/`PKCS12KeyStore`
/// both use a `LinkedHashMap` internally, so `KeyStore.aliases()` enumerates
/// entries in the order they were read from the file/inserted, not an
/// arbitrary hash order. Spring Boot's `SslInfo`
/// (`SslInfoTests.trustStoreCertificatesShouldProvideSslInfo` et al.) asserts
/// on that exact positional order (`getTrustStoreCertificateChains().get(0)`
/// is the FIRST entry in the file, not the alphabetically-first one) —
/// `std::collections::HashMap`'s randomized iteration order can't satisfy
/// that.
#[derive(Clone, Debug, Default)]
pub struct LoadedKeyStore {
    pub entries: IndexMap<String, KeyStoreEntry>,
}

/// Errors produced by the keystore parsers.
#[derive(Debug, Clone, thiserror::Error)]
pub enum KeyStoreError {
    #[error("not a recognised keystore (no PKCS#12 SEQUENCE prefix nor 0xFEEDFEED magic)")]
    UnknownFormat,
    #[error("keystore truncated at offset {0}")]
    Truncated(usize),
    #[error("JKS magic mismatch")]
    BadJksMagic,
    #[error("JKS unknown version {0}")]
    BadJksVersion(u32),
    #[error("JKS unknown entry tag {0}")]
    BadJksTag(u32),
    /// KS-4: real `JavaKeyStore`/`JceKeyStore` both throw this exact text on
    /// a MAC mismatch (`sun.security.provider.JavaKeyStore.engineLoad`,
    /// `com.sun.crypto.provider.JceKeyStore.engineLoad`) — a wrong password
    /// and a genuinely tampered file are indistinguishable from the MAC
    /// alone, so neither VM tries to tell them apart in the message.
    #[error("Keystore was tampered with, or password was incorrect")]
    JksMacMismatch,
    #[error("PKCS#12 parse failed: {0}")]
    Pkcs12Parse(String),
    #[error("PKCS#12 MAC verification failed (wrong password?)")]
    Pkcs12MacFailed,
    #[error("PKCS#12 shrouded key bag decrypt failed (wrong password?)")]
    Pkcs12KeyDecryptFailed,
    #[error("alias {0:?} not found")]
    UnknownAlias(String),
}

// ---------------------------------------------------------------------------
// Process-wide registry
// ---------------------------------------------------------------------------

static KEYSTORE_REGISTRY: OnceLock<RwLock<KeyStoreRegistry>> = OnceLock::new();

#[derive(Default)]
struct KeyStoreRegistry {
    next_id: i32,
    stores: HashMap<i32, LoadedKeyStore>,
    /// Store id -> the VM that registered it (gc-common w10-e). Ids come from
    /// one process-wide counter, so they never collide across VMs; this only
    /// decides which VM's teardown drops the store ([`forget_vm_keystores`]).
    /// A store registered through the VM-less [`keystore_register`] has no
    /// row and lives for the process, as every store did before.
    owners: HashMap<i32, usize>,
    /// A Java object's weak lock key -> the ids of its VM's own stores it
    /// carries (gc-common w14-f; see "Store lifetime" below). One row per
    /// carrier object: a `KeyStore` / `KeyStoreSpi` stamped by
    /// [`set_store_id`], a `PrivateKey` proxy [`engine_get_key`] packed the id
    /// into, a `tls.rs` `TrustManagerFactory` whose slot 2 names it. Each row
    /// goes when the lock-key sweep frees its key ([`forget_keystore_keys`]).
    /// Plain integers: no `ObjectRef` is held.
    ///
    /// Keyed `(vm, identity hash)` until gc-common w27-b, so two LIVE carriers
    /// of one VM with one hash shared a row: a re-stamp of one dropped the
    /// other's record of the id, and the store could be freed under it.
    carriers: HashMap<usize, StoreCarrierRow>,
    /// Store id -> how many [`KeyStoreRegistry::carriers`] rows name it.
    carrier_count: HashMap<i32, usize>,
    /// Stores whose last recorded carrier went: freed by
    /// [`release_store_candidates`] unless a factory row still names them.
    candidates: std::collections::HashSet<i32>,
    /// Stores stamped on an object with no identity hash, so a carrier this
    /// module cannot watch: never released while the VM lives (the pre-w14
    /// behaviour).
    pinned: std::collections::HashSet<i32>,
}

/// One [`KeyStoreRegistry::carriers`] row: the VM whose object carries the
/// ids (a freed key no longer names its VM, and the release that follows is
/// per VM), and the ids.
#[derive(Debug, Default)]
struct StoreCarrierRow {
    vm: usize,
    ids: Vec<i32>,
}

fn registry() -> &'static RwLock<KeyStoreRegistry> {
    KEYSTORE_REGISTRY.get_or_init(|| {
        RwLock::new(KeyStoreRegistry {
            next_id: 1,
            stores: HashMap::new(),
            owners: HashMap::new(),
            carriers: HashMap::new(),
            carrier_count: HashMap::new(),
            candidates: std::collections::HashSet::new(),
            pinned: std::collections::HashSet::new(),
        })
    })
}

/// Stash a parsed keystore and return its assigned id.
///
/// VM-less: the store outlives every VM, and is never released while one
/// lives either ([`release_store_candidates`] only frees a store with an
/// owner). Callers with a `ctx` use [`keystore_register_in_vm`]. The one
/// production VM-less caller is `tls.rs`'s `trust_store_keystore_id`, on
/// purpose: its id goes into a process-wide cache keyed by path, which every
/// VM reads (`handoff-w10e-tls-rs-trust-and-keystore-rows-carry-their-vm.md`,
/// "Deliberately NOT to change"). The others are tests.
pub fn keystore_register(store: LoadedKeyStore) -> i32 {
    let mut g = registry().write();
    let id = g.next_id;
    g.next_id = g.next_id.checked_add(1).unwrap_or(1);
    g.stores.insert(id, store);
    id
}

/// [`keystore_register`] for VM `vm`: the store is dropped with that VM
/// ([`forget_vm_keystores`]), or earlier, once nothing carries its id any
/// more (gc-common w14-f, "Store lifetime" below).
///
/// Also the retry point for VM `vm`'s release candidates that a factory row
/// held at the last attempt: the loop that grows this registry -- a fresh
/// `KeyStore` per reload -- comes through here once per iteration.
pub fn keystore_register_in_vm(vm: usize, store: LoadedKeyStore) -> i32 {
    let id = {
        let mut g = registry().write();
        let id = g.next_id;
        g.next_id = g.next_id.checked_add(1).unwrap_or(1);
        g.stores.insert(id, store);
        g.owners.insert(id, vm);
        id
    };
    release_store_candidates(vm);
    id
}

// ---------------------------------------------------------------------------
// Store lifetime (gc-common w14-f)
// ---------------------------------------------------------------------------
//
// A store id is carried by Java objects, and a store is only needed while one
// of them can still ask for it. The carriers, each recorded in
// `KeyStoreRegistry::carriers` under its weak lock key, whose freeing by the
// lock-key sweep (the object died) drops the row (gc-common w27-b; a `t27_tls`
// `StoreIdRow` weak owner of a `(vm, identity hash)` key before that):
//
// * the `KeyStore` / `KeyStoreSpi` object [`set_store_id`] stamps (a public
//   `java.security.KeyStore` wrapper reaches its store through its SPI, which
//   it holds strongly);
// * every `PrivateKey` proxy [`engine_get_key`] packs the id into
//   ([`private_key_der_from_proxy`] reads the DER back through it long after
//   the `KeyStore` may be gone);
// * `tls.rs`'s `TrustManagerFactory`, whose slot 2 holds the id until
//   `getTrustManagers()` builds from it.
//
// One more holder is not an object this module sees:
// `phases_late::ssl_security::kmf_keystore_id_by_identity`, the id a
// `KeyManagerFactory.init(KeyStore, char[])` recorded for a later
// `getKeyManagers()`. It is consulted by scan at release time
// ([`release_store_candidates`]); a store it holds stays a candidate and is
// retried at the next [`keystore_register_in_vm`] or carrier death.
//
// NOT holders, although they carry a `keystore_id`: `x509_manager`'s
// `KeyManagerState` / `TrustManagerState`. Both copy the store's contents
// when they are built (the w11-a snapshot audit) and read the id back only
// for a diagnostic line.
//
// When a carrier row goes and no other row names the id, the id becomes a
// candidate; [`release_store_candidates`] frees it unless it was carried
// again, is pinned, or a factory row names it. Only a store this VM
// registered is ever released: a VM-less one ([`keystore_register`], the
// process-wide default-truststore cache in `tls.rs`) never enters any of
// these tables. A carrier this module cannot watch (no identity hash) pins its
// store for the life of the VM, the behaviour before this change.

/// Record `carrier` (its CURRENT address: an argument, or a reference just
/// allocated or re-read from a pin, before anything that can allocate) as an
/// object that carries store `id` of the calling VM. Idempotent. A no-op for
/// an id this VM did not register.
pub fn note_store_carrier(ctx: &dyn NativeContext, carrier: ObjectRef, id: i32) {
    if id <= 0 || carrier.as_ptr().is_null() {
        return;
    }
    let vm = ctx.vm_identity();
    // A store this VM did not register is never recorded; ask before minting
    // a key for it (gc-common w27-b). Re-checked under the write lock below.
    if registry().read().owners.get(&id) != Some(&vm) {
        return;
    }
    // Before the lock: it may install a hash in the object's header, and the
    // key takes the lock-key registry. `None`: no identity hash (not a heap
    // object) -- a carrier this module cannot watch.
    let key = store_row_key(ctx, carrier);
    let mut g = registry().write();
    let reg = &mut *g;
    if reg.owners.get(&id) != Some(&vm) {
        return;
    }
    let Some(key) = key else {
        reg.pinned.insert(id);
        reg.candidates.remove(&id);
        return;
    };
    let row = reg.carriers.entry(key).or_insert_with(|| StoreCarrierRow {
        vm,
        ids: Vec::new(),
    });
    if !row.ids.contains(&id) {
        row.ids.push(id);
        *reg.carrier_count.entry(id).or_insert(0) += 1;
    }
    // Carried again: its next candidacy comes from this carrier's death.
    reg.candidates.remove(&id);
}

/// The key `obj`'s rows are filed under in [`store_id_by_identity`] and
/// [`KeyStoreRegistry::carriers`]: its weak lock key
/// (`crate::gc_stable_weak_lock_key`), MINTED -- or `None` for an object with
/// no identity hash (not a heap object; before gc-common w27-b such an object
/// had no identity row either, and pinned the stores it carried). Per VM and
/// per live object, stable across moves, never minted again once the sweep
/// frees it. Compute it BEFORE a table guard. `obj` must be CURRENT.
fn store_row_key(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<usize> {
    if ctx.identity_hash_code(obj) == 0 {
        return None;
    }
    crate::gc_stable_weak_lock_key(ctx, obj).ok()
}

/// `carrier` was stamped with another id and no longer carries `id`: drop its
/// record, and release the store if that was the last one.
///
/// Rows are keyed by weak lock keys since gc-common w27-b, so a LIVE carrier
/// that shares `carrier`'s identity hash keeps its own record (it used to
/// share the row and lose it here).
pub fn forget_store_carrier(ctx: &dyn NativeContext, carrier: ObjectRef, id: i32) {
    if id <= 0 || carrier.as_ptr().is_null() {
        return;
    }
    let vm = ctx.vm_identity();
    // Never mints: a carrier never keyed has no record.
    let Some(key) = crate::existing_weak_lock_key(ctx, carrier) else {
        return;
    };
    let offered = {
        let mut g = registry().write();
        let reg = &mut *g;
        let removed = match reg.carriers.get_mut(&key) {
            Some(row) => match row.ids.iter().position(|x| *x == id) {
                Some(pos) => {
                    row.ids.swap_remove(pos);
                    if row.ids.is_empty() {
                        reg.carriers.remove(&key);
                    }
                    true
                }
                None => false,
            },
            None => false,
        };
        removed && uncount_carrier(reg, id)
    };
    if offered {
        release_store_candidates(vm);
    }
}

/// One carrier row stopped naming `id`: decrement, and make `id` a candidate
/// when no row names it any more. Answers whether it did.
fn uncount_carrier(reg: &mut KeyStoreRegistry, id: i32) -> bool {
    let left = match reg.carrier_count.get_mut(&id) {
        Some(n) => {
            *n = n.saturating_sub(1);
            *n
        }
        None => 0,
    };
    if left == 0 {
        reg.carrier_count.remove(&id);
        reg.candidates.insert(id);
        true
    } else {
        false
    }
}

/// gc-common w27-b: drop every row this module filed under a weak lock key
/// the registry has just FREED -- its object died (`lib.rs::sweep_lock_keys`,
/// after every collection) or its VM went (`lib.rs::forget_vm_lock_keys`) --
/// and release the stores a dropped carrier row was the last carrier of.
/// `keys` holds every weak key the sweep freed, of every kind; most name no
/// row here. Answers the rows dropped (identity rows, carrier rows and the
/// stores released).
///
/// Replaces the `t27_tls` `StoreIdRow` weak owner (keyed `(vm, identity
/// hash)`, which two live same-hash carriers shared). A freed key is never
/// minted again, so nothing could read these rows any more.
///
/// Each VM that lost a carrier row has ALL its candidates retried, not only
/// those this sweep offered: a candidate a factory row held at the last
/// attempt is freed here once that factory has died. Locks one at a time --
/// the identity table, the registry, then (in [`release_store_candidates`])
/// the registry and the `ssl_security` factory table -- never nested, never
/// under the lock-key registry's lock (both callers release it first);
/// nothing here reaches Java.
pub fn forget_keystore_keys(keys: &[usize]) -> usize {
    if keys.is_empty() {
        return 0;
    }
    let mut key_set: Option<std::collections::HashSet<usize>> = None;
    let identity_rows = {
        let mut rows = store_id_by_identity()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = rows.len();
        if rows.len() < keys.len() {
            let set = key_set.get_or_insert_with(|| keys.iter().copied().collect());
            rows.retain(|k, _| !set.contains(k));
        } else {
            for k in keys {
                rows.remove(k);
            }
        }
        before - rows.len()
    };
    let mut vms: Vec<usize> = Vec::new();
    let carrier_rows = {
        let mut g = registry().write();
        let reg = &mut *g;
        if reg.carriers.is_empty() {
            return identity_rows;
        }
        let dead: Vec<usize> = if reg.carriers.len() < keys.len() {
            let set = key_set.get_or_insert_with(|| keys.iter().copied().collect());
            reg.carriers
                .keys()
                .filter(|k| set.contains(*k))
                .copied()
                .collect()
        } else {
            keys.iter()
                .filter(|k| reg.carriers.contains_key(*k))
                .copied()
                .collect()
        };
        for key in &dead {
            if let Some(row) = reg.carriers.remove(key) {
                for id in row.ids {
                    uncount_carrier(reg, id);
                }
                if !vms.contains(&row.vm) {
                    vms.push(row.vm);
                }
            }
        }
        dead.len()
    };
    let released: usize = vms.into_iter().map(release_store_candidates).sum();
    identity_rows + carrier_rows + released
}

/// Free VM `vm`'s candidate stores that no carrier, pin or factory row names
/// any more; answer how many went. Registry and factory table are locked one
/// at a time, never nested; nothing here calls into the VM.
///
/// Safe outside a pause: a candidate has no live carrier (every carrier was
/// judged dead by a collection, or re-stamped), so no mutator can reach its
/// id except through a factory row, which is checked, and a new carrier is
/// re-checked under the second acquisition.
///
/// Retried from [`keystore_register_in_vm`], from every carrier-row death
/// ([`forget_keystore_keys`]), from a re-stamp ([`forget_store_carrier`])
/// and from `x509_manager::forget_key_manager_state` (a keystore-backed
/// `KeyManagerState` freed means its factory's row just went).
pub fn release_store_candidates(vm: usize) -> usize {
    let mine: Vec<i32> = {
        let mut g = registry().write();
        let reg = &mut *g;
        if reg.candidates.is_empty() {
            return 0;
        }
        // Stale candidates: carried again (the carrier's death offers it
        // again), pinned, or already gone.
        let (owners, pinned, counts) = (&reg.owners, &reg.pinned, &reg.carrier_count);
        reg.candidates.retain(|id| {
            owners.contains_key(id) && !pinned.contains(id) && !counts.contains_key(id)
        });
        reg.candidates
            .iter()
            .copied()
            .filter(|id| reg.owners.get(id) == Some(&vm))
            .collect()
    };
    if mine.is_empty() {
        return 0;
    }
    // A `KeyManagerFactory` initialised on the store can still build from it
    // in `getKeyManagers()`.
    let named: std::collections::HashSet<i32> = {
        let rows = crate::phases_late::ssl_security::kmf_keystore_id_by_identity().lock();
        rows.iter()
            .filter(|((owner, _), _)| *owner == vm)
            .map(|(_, ks)| *ks)
            .collect()
    };
    let freed: Vec<i32> = {
        let mut g = registry().write();
        let reg = &mut *g;
        let mut freed = Vec::new();
        for id in mine {
            if named.contains(&id)
                || !reg.candidates.contains(&id)
                || reg.carrier_count.contains_key(&id)
                || reg.pinned.contains(&id)
                || reg.owners.get(&id) != Some(&vm)
            {
                continue;
            }
            reg.candidates.remove(&id);
            reg.owners.remove(&id);
            reg.stores.remove(&id);
            freed.push(id);
        }
        freed
    };
    if !freed.is_empty() {
        let mut pems = store_identity_pem_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for id in &freed {
            pems.remove(id);
        }
    }
    freed.len()
}

/// Replace the contents of store `id` in place, if VM `vm` registered it
/// (gc-common w11-a). Answers whether it did; `false` leaves the registry
/// untouched, hands the store back, and the caller registers a new one.
///
/// `KeyStore.load` on an object that already has a store used to mint a NEW
/// id every time, and nothing removes the old one while the VM lives: a server
/// that reloads the same `KeyStore` object on a timer kept every earlier copy
/// of its private keys. Reusing the id is what a reload means -- the JDK's
/// `engineLoad` clears the SPI's own entry map and refills it. A store another
/// VM registered, or one registered VM-lessly, is never overwritten.
pub fn keystore_replace_in_vm(
    vm: usize,
    id: i32,
    store: LoadedKeyStore,
) -> Result<(), LoadedKeyStore> {
    if id == 0 {
        return Err(store);
    }
    let mut g = registry().write();
    if g.owners.get(&id) != Some(&vm) || !g.stores.contains_key(&id) {
        return Err(store);
    }
    g.stores.insert(id, store);
    Ok(())
}

/// Drop every keystore VM `vm` registered, with the identity rows and
/// identity PEMs filed under it (VM teardown, from
/// `t27_tls::forget_vm_tls_state`; gc-common w10-e). A keystore holds key
/// material, so a torn-down VM's stores lingering for the life of the process
/// was a leak of private keys as well as of memory.
pub fn forget_vm_keystores(vm: usize) {
    let dropped: Vec<i32> = {
        let mut g = registry().write();
        let ids: Vec<i32> = g
            .owners
            .iter()
            .filter(|(_, owner)| **owner == vm)
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            g.owners.remove(id);
            g.stores.remove(id);
            // gc-common w14-f: the lifetime bookkeeping of the same stores.
            g.carrier_count.remove(id);
            g.candidates.remove(id);
            g.pinned.remove(id);
        }
        // Only this VM's own stores are ever recorded as carried, and only
        // by this VM's objects, so its rows name nothing another VM counts.
        g.carriers.retain(|_, row| row.vm != vm);
        ids
    };
    store_id_by_identity()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, row| row.vm != vm);
    if !dropped.is_empty() {
        let mut pems = store_identity_pem_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for id in &dropped {
            pems.remove(id);
        }
    }
}

/// Look up a parsed keystore by id.
pub fn keystore_lookup(id: i32) -> Option<LoadedKeyStore> {
    registry().read().stores.get(&id).cloned()
}

/// Answer `f` over store `id` under the registry's read lock, WITHOUT cloning
/// the store; `None` when there is no such store.
///
/// gc-common w33-c: the `engine*` accessors used to take a whole-store clone
/// ([`keystore_lookup`]) for one boolean or one entry. A truststore walk --
/// the JDK's own `aliases()` then `isCertificateEntry(a)` / `getCertificate(a)`
/// per alias (`KeyStores.getTrustedCerts`, every `TrustManagerFactory` over
/// `cacerts`) -- copied the full ~150-certificate store twice per alias,
/// quadratic in the store. `f` must only read Rust data: it runs under the
/// lock, so it must not call into the VM, allocate on the Java heap or take
/// another lock.
fn with_store<R>(id: i32, f: impl FnOnce(&LoadedKeyStore) -> R) -> Option<R> {
    registry().read().stores.get(&id).map(f)
}

/// Insert/replace a trusted-cert entry in an already-registered store
/// (in-memory `KeyStore.setCertificateEntry`). Reads and writes share this
/// side-table, so the entry is visible to `engineAliases`/`engineSize`/
/// `engineGetCertificate`. Returns true if the store existed.
pub fn keystore_set_cert_entry(id: i32, alias: &str, cert_der: Vec<u8>) -> bool {
    let mut g = registry().write();
    if let Some(store) = g.stores.get_mut(&id) {
        store.entries.insert(
            alias.to_string(),
            KeyStoreEntry {
                alias: alias.to_string(),
                creation_time_ms: entry_set_time_ms(),
                kind: EntryKind::TrustedCert { cert_der },
            },
        );
        true
    } else {
        false
    }
}

/// Insert/replace a private-key entry in an already-registered store
/// (in-memory `KeyStore.setKeyEntry(String, Key, char[], Certificate[])`).
/// Companion to `keystore_set_cert_entry` for the PrivateKey case -- same
/// rationale: the real `PKCS12KeyStoreSpi.engineSetKeyEntry` bytecode (when
/// reached without a native override) mutates the real SPI object's own
/// `entries` field, invisible to CratonVM's side-table-backed reads
/// (`engineAliases`/`engineSize`/`keystore_get_private_key`) and, critically,
/// to `keystore_set_pending_km_identity` -- so `KeyManagerFactory.init`
/// found no identity to stage for the TLS layer. Returns true if the store
/// existed.
pub fn keystore_set_key_entry(
    id: i32,
    alias: &str,
    key_der: Vec<u8>,
    chain: Vec<Vec<u8>>,
    protected: Option<Vec<u8>>,
) -> bool {
    let mut g = registry().write();
    if let Some(store) = g.stores.get_mut(&id) {
        store.entries.insert(
            alias.to_string(),
            KeyStoreEntry {
                alias: alias.to_string(),
                creation_time_ms: entry_set_time_ms(),
                kind: EntryKind::PrivateKey {
                    key_der,
                    chain,
                    protected,
                },
            },
        );
        true
    } else {
        false
    }
}

/// Insert/replace a secret-key entry in an already-registered store
/// (`KeyStore.setEntry` with a `SecretKeyEntry`, or the pre-1.5
/// `setKeyEntry(alias, SecretKey, password, null)`).
///
/// Companion to `keystore_set_key_entry` for the symmetric case. Before it
/// existed, both routes either did nothing at all (`setEntry` -- see
/// `engine_set_entry`) or stored the raw bytes as a `PrivateKey` entry, so
/// `getKey` handed back a `PrivateKey` proxy claiming algorithm RSA for what
/// the caller had put in as an AES `SecretKeySpec`.
pub fn keystore_set_secret_key_entry(
    id: i32,
    alias: &str,
    key_bytes: Vec<u8>,
    algorithm: &str,
) -> bool {
    let mut g = registry().write();
    if let Some(store) = g.stores.get_mut(&id) {
        store.entries.insert(
            alias.to_string(),
            KeyStoreEntry {
                alias: alias.to_string(),
                creation_time_ms: entry_set_time_ms(),
                kind: EntryKind::SecretKey {
                    key_bytes,
                    algorithm: algorithm.to_string(),
                },
            },
        );
        true
    } else {
        false
    }
}

/// The creation date of an entry set through the API: now.
///
/// gc-common w34-c discovery. The three setters above stamped `0`, so
/// `getCreationDate(alias)` of an entry just put in with `setCertificateEntry`
/// / `setKeyEntry` / `setEntry` answered the epoch (rendered as a 1969/1970
/// local date), and a JKS store written from it carried that date to disk.
/// HotSpot 25.0.3 stamps `new Date()` in all three store types (measured:
/// `getCreationDate("c").getTime() > 0` is true for JKS, PKCS12 and JCEKS).
/// Plain wall-clock milliseconds, as `Date` holds; `0` only if the clock is
/// before the epoch.
fn entry_set_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Remove an entry from a registered store (`KeyStore.deleteEntry`).
pub fn keystore_delete_entry(id: i32, alias: &str) {
    let mut g = registry().write();
    if let Some(store) = g.stores.get_mut(&id) {
        // `shift_remove`, not `swap_remove`: preserves the remaining
        // entries' relative order (matches a real `LinkedHashMap.remove`),
        // consistent with `LoadedKeyStore::entries`'s doc comment.
        store.entries.shift_remove(alias);
    }
}

/// Convenience for the TLS layer: fetch the PKCS#8 private-key DER for an
/// alias, plus the cert chain (leaf first), without exposing the registry
/// internals. Returns `None` if no `PrivateKey` entry exists.
pub fn keystore_get_private_key(id: i32, alias: &str) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    // The one entry is copied out, not the whole store (gc-common w34-c, the
    // `with_store` rule of w33-c: this cloned every entry of the store per
    // call, and `tls.rs`'s synthetic `KeyStore.getKey` calls it per alias).
    with_store(id, |store| match &store.entries.get(alias)?.kind {
        EntryKind::PrivateKey { key_der, chain, .. } => Some((key_der.clone(), chain.clone())),
        _ => None,
    })
    .flatten()
}

/// Convenience for the TrustManager layer: fetch the cert DER for a trusted
/// cert entry, or the leaf cert for a private-key entry.
pub fn keystore_get_cert_der(id: i32, alias: &str) -> Option<Vec<u8>> {
    // One DER copied out, not the store (gc-common w34-c, see above).
    with_store(id, |store| match &store.entries.get(alias)?.kind {
        EntryKind::TrustedCert { cert_der } => Some(cert_der.clone()),
        EntryKind::PrivateKey { chain, .. } => chain.first().cloned(),
        EntryKind::SecretKey { .. } => None,
    })
    .flatten()
}

// ---------------------------------------------------------------------------
// Format detection + dispatch
// ---------------------------------------------------------------------------

/// JKS magic constant `0xFEEDFEED` (file's first 4 bytes, big-endian).
pub const JKS_MAGIC: u32 = 0xFEEDFEED;

/// JCEKS magic constant `0xCECECECE`.
///
/// JCEKS is JKS's record layout under a different magic and with a stronger
/// key-protection PBE. This VM's JKS path does not decrypt private keys, so the
/// two are the same parser here and the magic is the whole difference --
/// MEASURED against HotSpot by `KeyStoreTypeProbe.java`, whose
/// `[JCEKS] stored magic` row is `cececece`.
pub const JCEKS_MAGIC: u32 = 0xCECE_CECE;

/// JKS HMAC salt. The construction is `SHA1(passwd_utf16be || SALT || body)`.
const JKS_HMAC_SALT: &[u8] = b"Mighty Aphrodite";

/// Detect format and load. `password` is the UTF-16-style password real-JDK
/// hands us as a `char[]`; both parsers receive the raw bytes the user typed
/// (UTF-8 of those chars) so they can apply their per-format mixing.
pub fn load_keystore(bytes: &[u8], password: &[u8]) -> Result<LoadedKeyStore, KeyStoreError> {
    load_keystore_ex(bytes, password, true)
}

/// `verify_mac=false` mirrors real-JDK's `KeyStore.load(stream, null)`
/// contract: a Java `null` password (as opposed to an empty `char[]`)
/// disables PKCS#12 integrity checking entirely rather than checking against
/// an empty password. Callers that can't distinguish "no password supplied"
/// from "empty password supplied" should keep using [`load_keystore`].
pub fn load_keystore_ex(
    bytes: &[u8],
    password: &[u8],
    verify_mac: bool,
) -> Result<LoadedKeyStore, KeyStoreError> {
    if bytes.len() < 4 {
        return Err(KeyStoreError::Truncated(0));
    }
    let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if magic == JKS_MAGIC || magic == JCEKS_MAGIC {
        load_jks(bytes, password)
    } else if bytes[0] == 0x30 {
        load_pkcs12_ex(bytes, password, verify_mac)
    } else {
        Err(KeyStoreError::UnknownFormat)
    }
}

// ---------------------------------------------------------------------------
// PKCS#12 — backed by the `p12` crate
// ---------------------------------------------------------------------------

/// JCA secret-key algorithm name -> the OID `SunPKCS12` writes into a
/// SecretBag's inner `AlgorithmIdentifier`.
///
/// MEASURED on JDK 25, not guessed: this is exactly what
/// `AlgorithmId.get(name).getOID()` answers, and two of the rows are not the
/// OID an educated guess produces (`DESede` is the OIW `1.3.14.3.2.17`, not
/// PKCS#3's `des-EDE3-CBC`; `Blowfish` is `…3029.1.1.2`, not `…3029.1.2`).
/// Getting one wrong is not a parse error on either side — the key material
/// still round-trips — it just comes back out under a DIFFERENT algorithm
/// name, which is the quiet kind of wrong this whole record is about.
const SECRET_KEY_ALG_OIDS: &[(&str, &[u64])] = &[
    ("AES", &[2, 16, 840, 1, 101, 3, 4, 1]),
    ("DESede", &[1, 3, 14, 3, 2, 17]),
    ("DES", &[1, 3, 14, 3, 2, 7]),
    ("RC2", &[1, 2, 840, 113549, 3, 2]),
    ("ARCFOUR", &[1, 2, 840, 113549, 3, 4]),
    ("Blowfish", &[1, 3, 6, 1, 4, 1, 3029, 1, 1, 2]),
    ("HmacSHA1", &[1, 2, 840, 113549, 2, 7]),
    ("HmacSHA224", &[1, 2, 840, 113549, 2, 8]),
    ("HmacSHA256", &[1, 2, 840, 113549, 2, 9]),
    ("HmacSHA384", &[1, 2, 840, 113549, 2, 10]),
    ("HmacSHA512", &[1, 2, 840, 113549, 2, 11]),
];

/// OID -> the name to report on the way back OUT, which is what
/// `AlgorithmId.getName()` answers on JDK 25 — also measured.
///
/// This is deliberately NOT the inverse of [`SECRET_KEY_ALG_OIDS`]: the JDK is
/// itself asymmetric for three algorithms (`AlgorithmId.get("DES")` encodes
/// `1.3.14.3.2.7`, but reading that OID back names it `"DES/CBC"`). Matching
/// the JDK's answer beats internal tidiness here, because the thing that
/// compares the two is a differential against HotSpot. The `des-EDE3-CBC` row
/// has no write counterpart at all; it exists because OpenSSL-produced files
/// use it.
const SECRET_KEY_ALG_NAMES: &[(&[u64], &str)] = &[
    (&[2, 16, 840, 1, 101, 3, 4, 1], "AES"),
    (&[1, 3, 14, 3, 2, 17], "DESede"),
    (&[1, 2, 840, 113549, 3, 7], "DESede/CBC/NoPadding"),
    (&[1, 3, 14, 3, 2, 7], "DES/CBC"),
    (&[1, 2, 840, 113549, 3, 2], "RC2/CBC/PKCS5Padding"),
    (&[1, 2, 840, 113549, 3, 4], "ARCFOUR"),
    (&[1, 3, 6, 1, 4, 1, 3029, 1, 1, 2], "Blowfish"),
    (&[1, 2, 840, 113549, 2, 7], "HmacSHA1"),
    (&[1, 2, 840, 113549, 2, 8], "HmacSHA224"),
    (&[1, 2, 840, 113549, 2, 9], "HmacSHA256"),
    (&[1, 2, 840, 113549, 2, 10], "HmacSHA384"),
    (&[1, 2, 840, 113549, 2, 11], "HmacSHA512"),
];

/// Parse a dotted OID string (`"1.2.840.113549.3.7"`) into components.
///
/// Rejects anything that is not a well-formed OID, including the arc-0/1
/// constraint the DER encoder relies on — a bad first arc would panic the
/// writer rather than produce a wrong file, which is worse.
fn parse_dotted_oid(text: &str) -> Option<Vec<u64>> {
    let parts: Option<Vec<u64>> = text.split('.').map(|c| c.parse::<u64>().ok()).collect();
    let parts = parts?;
    if parts.len() < 2 || parts[0] > 2 || (parts[0] < 2 && parts[1] >= 40) {
        return None;
    }
    Some(parts)
}

/// The OID to encode a JCA secret-key algorithm name under, or `None` when this
/// VM cannot encode it at all (which is also true of a real JDK — see
/// `AlgorithmId.get("ChaCha20")`, a `NoSuchAlgorithmException`).
///
/// Three name shapes resolve, in order: a name this VM writes
/// ([`SECRET_KEY_ALG_OIDS`]); a name this VM *reads* ([`SECRET_KEY_ALG_NAMES`]),
/// so `load` → `store` of a `"DES/CBC"` entry is lossless rather than fatal;
/// and a bare dotted OID, which is what [`secret_key_alg_name`] answers for an
/// OID neither table names — same reason.
fn secret_key_alg_oid(name: &str) -> Option<yasna::models::ObjectIdentifier> {
    if let Some((_, oid)) = SECRET_KEY_ALG_OIDS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        return Some(yasna::models::ObjectIdentifier::from_slice(oid));
    }
    if let Some((oid, _)) = SECRET_KEY_ALG_NAMES
        .iter()
        .find(|(_, n)| n.eq_ignore_ascii_case(name))
    {
        return Some(yasna::models::ObjectIdentifier::from_slice(oid));
    }
    parse_dotted_oid(name).map(|parts| yasna::models::ObjectIdentifier::from_slice(&parts))
}

/// The JCA algorithm name for an OID read out of a SecretBag, falling back to
/// the dotted OID text (what `AlgorithmId.getName()` answers for an OID it has
/// no name for).
fn secret_key_alg_name(oid: &yasna::models::ObjectIdentifier) -> String {
    let components = oid.components().as_slice();
    for (known, name) in SECRET_KEY_ALG_NAMES {
        if components == *known {
            return (*name).to_string();
        }
    }
    components
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// [`secret_key_alg_name`] over a parsed `p12::AlgorithmIdentifier`.
///
/// `AlgorithmIdentifier::parse` folds three legacy PKCS#12 OIDs into named
/// variants before anything reaches `OtherAlg`. None of them is a secret-key
/// algorithm, but a malformed/hostile file could still put one there, so name
/// them explicitly instead of falling through to a wrong default.
fn secret_alg_name(alg: &p12::AlgorithmIdentifier) -> String {
    match alg {
        p12::AlgorithmIdentifier::OtherAlg(other) => secret_key_alg_name(&other.algorithm_type),
        p12::AlgorithmIdentifier::Sha1 => "SHA1".to_string(),
        p12::AlgorithmIdentifier::PbewithSHAAnd40BitRC2CBC(_) => "PBEWithSHA1AndRC2_40".to_string(),
        p12::AlgorithmIdentifier::PbeWithSHAAnd3KeyTripleDESCBC(_) => {
            "PBEWithSHA1AndDESede".to_string()
        }
    }
}

/// Crate-private `p12::bmp_string`, re-implemented: UTF-16BE + trailing 0x0000.
/// The PKCS#12 PBE/MAC password mixing operates on this BMPString form.
fn pkcs12_bmp_string(s: &str) -> Vec<u8> {
    let utf16: Vec<u16> = s.encode_utf16().collect();
    let mut bytes = Vec::with_capacity(utf16.len() * 2 + 2);
    for c in utf16 {
        bytes.push((c >> 8) as u8);
        bytes.push((c & 0xff) as u8);
    }
    bytes.push(0x00);
    bytes.push(0x00);
    bytes
}

/// The `p12` crate's built-in MAC verifier only implements SHA-1.  That was
/// correct for the crate's own legacy fixtures, but current SunPKCS12 emits a
/// SHA-256 `MacData` by default.  Keep the PKCS#12 KDF local so we can verify
/// the digest recorded by the file rather than silently treating every MAC as
/// SHA-1.
#[derive(Clone, Copy)]
enum Pkcs12MacDigest {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl Pkcs12MacDigest {
    fn output_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha224 => 28,
            Self::Sha256 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
        }
    }

    fn block_len(self) -> usize {
        match self {
            Self::Sha1 | Self::Sha224 | Self::Sha256 => 64,
            Self::Sha384 | Self::Sha512 => 128,
        }
    }

    fn hash(self, bytes: &[u8]) -> Vec<u8> {
        use sha1::Digest as _;

        match self {
            Self::Sha1 => sha1::Sha1::digest(bytes).to_vec(),
            Self::Sha224 => sha2::Sha224::digest(bytes).to_vec(),
            Self::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(bytes).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
        }
    }
}

fn pkcs12_mac_digest(algorithm: &p12::AlgorithmIdentifier) -> Option<Pkcs12MacDigest> {
    use p12::AlgorithmIdentifier::{OtherAlg, Sha1};

    match algorithm {
        Sha1 => Some(Pkcs12MacDigest::Sha1),
        OtherAlg(other) => match other.algorithm_type.components().as_slice() {
            // NIST SHA-2 digest OIDs, as used by JDK 8u191+ SunPKCS12.
            [2, 16, 840, 1, 101, 3, 4, 2, 4] => Some(Pkcs12MacDigest::Sha224),
            [2, 16, 840, 1, 101, 3, 4, 2, 1] => Some(Pkcs12MacDigest::Sha256),
            [2, 16, 840, 1, 101, 3, 4, 2, 2] => Some(Pkcs12MacDigest::Sha384),
            [2, 16, 840, 1, 101, 3, 4, 2, 3] => Some(Pkcs12MacDigest::Sha512),
            _ => None,
        },
        _ => None,
    }
}

fn pkcs12_mac_kdf(
    digest: Pkcs12MacDigest,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    id: u8,
    output_len: usize,
) -> Vec<u8> {
    let v = digest.block_len();
    let u = digest.output_len();
    let repeat_to_block = |input: &[u8]| {
        if input.is_empty() {
            return Vec::new();
        }
        let len = v * input.len().div_ceil(v);
        input.iter().copied().cycle().take(len).collect::<Vec<_>>()
    };

    let mut i = repeat_to_block(salt);
    i.extend(repeat_to_block(password));
    let d = vec![id; v];
    let mut out = Vec::with_capacity(output_len);

    while out.len() < output_len {
        let mut round = Vec::with_capacity(d.len() + i.len());
        round.extend_from_slice(&d);
        round.extend_from_slice(&i);
        let mut a = digest.hash(&round);
        for _ in 1..iterations.max(1) {
            a = digest.hash(&a);
        }

        let b = a.iter().copied().cycle().take(v).collect::<Vec<_>>();
        for block in i.chunks_exact_mut(v) {
            let mut carry = 1u16;
            for (byte, addend) in block.iter_mut().rev().zip(b.iter().rev()) {
                let sum = *byte as u16 + *addend as u16 + carry;
                *byte = sum as u8;
                carry = sum >> 8;
            }
        }
        out.extend_from_slice(&a);
    }
    out.truncate(output_len);
    out
}

fn pkcs12_hmac(digest: Pkcs12MacDigest, key: &[u8], data: &[u8]) -> Vec<u8> {
    let block_len = digest.block_len();
    let mut padded_key = if key.len() > block_len {
        digest.hash(key)
    } else {
        key.to_vec()
    };
    padded_key.resize(block_len, 0);

    let mut inner = Vec::with_capacity(block_len + data.len());
    inner.extend(padded_key.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(data);
    let inner_hash = digest.hash(&inner);

    let mut outer = Vec::with_capacity(block_len + inner_hash.len());
    outer.extend(padded_key.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    digest.hash(&outer)
}

fn verify_pkcs12_mac(pfx: &p12::PFX, password: &str) -> bool {
    let Some(mac_data) = &pfx.mac_data else {
        return true;
    };
    let Some(digest) = pkcs12_mac_digest(&mac_data.mac.digest_algorithm) else {
        return false;
    };
    let password = pkcs12_bmp_string(password);
    let Some(auth_safe) = pfx.auth_safe.data(&password) else {
        return false;
    };
    let key = pkcs12_mac_kdf(
        digest,
        &password,
        &mac_data.salt,
        mac_data.iterations,
        3,
        digest.output_len(),
    );
    constant_time_eq(&pkcs12_hmac(digest, &key, &auth_safe), &mac_data.mac.digest)
}

/// BER-mode equivalent of `p12::PFX::bags`. Identical structure to the crate's
/// own `bags()` (auth_safe -> SEQUENCE OF ContentInfo -> per-content data ->
/// SEQUENCE OF SafeBag) but parsed with `yasna::parse_ber`, which does not
/// enforce DER canonical SET-OF ordering. The JDK emits trusted-cert bag
/// attribute sets out of DER order (friendlyName before the Oracle
/// trustedKeyUsage attribute); SunJSSE accepts them and so must we. BER is a
/// strict superset of DER — every field is still fully decoded and type-checked;
/// only the DER-only canonical-ordering constraint is relaxed (kcfull #12).
fn bags_ber(
    pfx: &p12::PFX,
    password_str: &str,
    tolerate_undecryptable: bool,
) -> Result<Vec<p12::SafeBag>, yasna::ASN1Error> {
    let password = pkcs12_bmp_string(password_str);
    let data = pfx
        .auth_safe
        .data(&password)
        .ok_or_else(|| yasna::ASN1Error::new(yasna::ASN1ErrorKind::Invalid))?;
    let contents = yasna::parse_ber(&data, |r| r.collect_sequence_of(p12::ContentInfo::parse))?;
    let mut result = Vec::new();
    for content in contents.iter() {
        let decrypted = matches!(content, p12::ContentInfo::EncryptedData(_));
        let inner = match content {
            p12::ContentInfo::Data(data) => Some(data.clone()),
            p12::ContentInfo::EncryptedData(encrypted) => encrypted
                .data(&password)
                // `p12` 0.6 only decrypts the legacy PKCS#12 PBE algorithms.
                // Current SunPKCS12 encrypts certificate SafeContents with
                // PBES2/PBKDF2/AES, so use the recorded PBES2 parameters when
                // the crate deliberately returns None for that newer form.
                .or_else(|| {
                    decrypt_pbes2_content(
                        &encrypted.encrypted_content_info,
                        password_str.as_bytes(),
                    )
                    .or_else(|| decrypt_pbes2_content(&encrypted.encrypted_content_info, &password))
                }),
            p12::ContentInfo::OtherContext(_) => None,
        };
        let Some(inner) = inner else {
            // Real-JDK's PKCS12KeyStore loads each top-level AuthenticatedSafe
            // section independently and silently drops any section it can't
            // decrypt with the password it was given — this is how
            // `KeyStore.load(stream, null)` can still expose a keystore's
            // PrivateKeyEntry (carried, undecrypted, in an unencrypted outer
            // SafeContents; see the caller's `verify_mac=false` contract) even
            // though a *different* AuthenticatedSafe section (e.g. the
            // certificate SafeContents) is separately encrypted under the
            // store password we don't have. Only tolerate this when we
            // already know we lack a trustworthy password (`verify_mac` was
            // false) — with a MAC-verified password a decrypt failure here is
            // a real bug, not a missing-password situation, and must surface.
            if tolerate_undecryptable {
                continue;
            }
            return Err(yasna::ASN1Error::new(yasna::ASN1ErrorKind::Invalid));
        };
        // gc-common w33-c: these ciphers are unauthenticated too (see
        // `open_pkcs12_envelope`), so without a verified MAC a wrong password
        // "decrypts" an encrypted section to noise about one time in 128. That
        // section was not decryptable, and is skipped like one; it used to
        // fail the whole load.
        let safe_bags =
            match yasna::parse_ber(&inner, |r| r.collect_sequence_of(p12::SafeBag::parse)) {
                Ok(bags) => bags,
                Err(_) if tolerate_undecryptable && decrypted => continue,
                Err(e) => return Err(e),
            };
        result.extend(safe_bags);
    }
    Ok(result)
}

/// Decrypt the PBES2/PBKDF2/AES records emitted by current SunPKCS12 for
/// `SecretKeyEntry` values. `p12` itself supports only legacy PKCS#12 PBE.
fn decrypt_pbes2_params(params_der: &[u8], ciphertext: &[u8], password: &[u8]) -> Option<Vec<u8>> {
    let (salt, iterations, key_len, prf, iv) = yasna::parse_ber(params_der, |r| {
        r.read_sequence(|r| {
            let (salt, iterations, key_len, prf) = r.next().read_sequence(|r| {
                let _pbkdf2_oid = r.next().read_oid()?;
                r.next().read_sequence(|r| {
                    let salt = r.next().read_bytes()?;
                    let iterations = r.next().read_u32()?;
                    let key_len = r.read_optional(|r| r.read_u32())?.unwrap_or(32) as usize;
                    let prf = r.read_optional(|r| {
                        r.read_sequence(|r| {
                            let prf_oid = r.next().read_oid()?;
                            r.read_optional(|r| r.read_null())?;
                            Ok(prf_oid)
                        })
                    })?;
                    let prf = match prf.as_ref().map(|oid| oid.components().as_slice()) {
                        Some([1, 2, 840, 113549, 2, 7]) | None => 1,
                        Some([1, 2, 840, 113549, 2, 8]) => 224,
                        Some([1, 2, 840, 113549, 2, 9]) => 256,
                        Some([1, 2, 840, 113549, 2, 10]) => 384,
                        Some([1, 2, 840, 113549, 2, 11]) => 512,
                        _ => return Err(yasna::ASN1Error::new(yasna::ASN1ErrorKind::Invalid)),
                    };
                    Ok((salt, iterations, key_len, prf))
                })
            })?;
            let iv = r.next().read_sequence(|r| {
                let _aes_oid = r.next().read_oid()?;
                r.next().read_bytes()
            })?;
            Ok((salt, iterations, key_len, prf, iv))
        })
    })
    .ok()?;

    let key = crate::phases_early::pbkdf2_derive_for(prf, password, &salt, iterations, key_len);
    use aes::cipher::block_padding::Pkcs7;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecryptMut, KeyIvInit};
    let mut out = vec![0u8; ciphertext.len()];
    let written = match key.len() {
        16 => cbc::Decryptor::<aes::Aes128>::new(
            GenericArray::from_slice(&key),
            GenericArray::from_slice(&iv),
        )
        .decrypt_padded_b2b_mut::<Pkcs7>(&ciphertext, &mut out)
        .ok()?
        .len(),
        24 => cbc::Decryptor::<aes::Aes192>::new(
            GenericArray::from_slice(&key),
            GenericArray::from_slice(&iv),
        )
        .decrypt_padded_b2b_mut::<Pkcs7>(&ciphertext, &mut out)
        .ok()?
        .len(),
        32 => cbc::Decryptor::<aes::Aes256>::new(
            GenericArray::from_slice(&key),
            GenericArray::from_slice(&iv),
        )
        .decrypt_padded_b2b_mut::<Pkcs7>(&ciphertext, &mut out)
        .ok()?
        .len(),
        _ => return None,
    };
    out.truncate(written);
    Some(out)
}

fn decrypt_pbes2_content(content: &p12::EncryptedContentInfo, password: &[u8]) -> Option<Vec<u8>> {
    let p12::AlgorithmIdentifier::OtherAlg(algorithm) = &content.content_encryption_algorithm
    else {
        return None;
    };
    decrypt_pbes2_params(
        algorithm.params.as_deref()?,
        &content.encrypted_content,
        password,
    )
}

fn decrypt_secret_pbes2(epk: &p12::EncryptedPrivateKeyInfo, password: &[u8]) -> Option<Vec<u8>> {
    let p12::AlgorithmIdentifier::OtherAlg(algorithm) = &epk.encryption_algorithm else {
        return None;
    };
    decrypt_pbes2_params(algorithm.params.as_deref()?, &epk.encrypted_data, password)}

/// Open a PKCS#12 key or secret envelope with `password`, accepting a
/// plaintext only when `accept` recognises it.
///
/// gc-common w33-c (`common-w25-keystore-pkcs12-setentry-rarely-reads-back-
/// unequal-on-zgc`): every cipher behind this -- the `p12` crate's legacy
/// 3DES/RC2 PBE, [`decrypt_pbes2_params`]' AES-CBC -- is unauthenticated, so
/// the only thing that ever told a wrong password from the right one was the
/// PKCS#7 padding check. A wrong key decrypts to noise whose padding is valid
/// with probability ~1/255 per attempt (last byte 0x01, or 0x02 0x02, ...),
/// and the loaders make two PBES2 attempts per bag. So about one wrong-password
/// open in 128 SUCCEEDED with garbage: a store written with an entry password
/// and reloaded with the store password (`LTKeyStoreEngineSweep`'s
/// `setEntry` and `setKeyEntry` rows, KS-5/KS-6's whole point) took that
/// garbage as the key's plaintext, `getKey` then had no envelope left to open
/// with the right password, and the key came back unequal. The envelope's salt
/// and IV are random per `store()`, so it was ~1% of runs, on every backend.
/// SunPKCS12 is immune because it parses the plaintext (a `PKCS8EncodedKeySpec`
/// / `SecretKeySpec` build) before believing it; `accept` is that parse.
///
/// Attempt order is the loaders' historical one: legacy PBE with the BMP
/// password, then PBES2 with the password's UTF-8 bytes, then with its BMP
/// form.
fn open_pkcs12_envelope<T>(
    epk: &p12::EncryptedPrivateKeyInfo,
    password: &[u8],
    password_bmp: &[u8],
    accept: impl Fn(&[u8]) -> Option<T>,
) -> Option<T> {
    if let Some(t) = epk.decrypt(password_bmp).and_then(|p| accept(p.as_slice())) {
        return Some(t);
    }
    if let Some(t) = decrypt_secret_pbes2(epk, password).and_then(|p| accept(p.as_slice())) {
        return Some(t);
    }
    decrypt_secret_pbes2(epk, password_bmp).and_then(|p| accept(p.as_slice()))
}

/// `plain` if it is a PKCS#8 `PrivateKeyInfo` -- the only thing a shrouded key
/// bag or a key-protector envelope may hold -- else `None`. The acceptor
/// [`open_pkcs12_envelope`] and [`recover_key_any_scheme`] use for keys.
///
/// `SEQUENCE { INTEGER version (0 or 1), AlgorithmIdentifier SEQUENCE { OID,
/// params OPTIONAL }, OCTET STRING privateKey, ... }`, spanning the whole
/// input; the RFC 5958 trailing `[0]` attributes / `[1]` public key are
/// accepted as any well-formed elements. BER, so a BouncyCastle key with
/// indefinite lengths still opens. Random bytes pass this with a probability
/// far below 2^-40 (an exact outer length, then four fixed tags).
fn pkcs8_private_key_info(plain: &[u8]) -> Option<Vec<u8>> {
    let parsed = yasna::parse_ber(plain, |r| {
        r.read_sequence(|r| {
            let version = r.next().read_u8()?;
            if version > 1 {
                return Err(yasna::ASN1Error::new(yasna::ASN1ErrorKind::Invalid));
            }
            r.next().read_sequence(|r| {
                let _algorithm = r.next().read_oid()?;
                let _params = r.read_optional(|r| read_element_unless_eoc(r))?;
                Ok(())
            })?;
            let _private_key = r.next().read_bytes()?;
            while r.read_optional(|r| read_element_unless_eoc(r))?.is_some() {}
            Ok(())
        })
    });
    parsed.ok().map(|()| plain.to_vec())
}

/// One whole element as DER, refusing (without consuming) an end-of-contents
/// marker, so an OPTIONAL / trailing-element read inside an indefinite-length
/// SEQUENCE stops at its end instead of swallowing the marker.
fn read_element_unless_eoc(r: yasna::BERReader<'_, '_>) -> yasna::ASN1Result<Vec<u8>> {
    let tag = r.lookahead_tag()?;
    if tag.tag_class == yasna::TagClass::Universal && tag.tag_number == 0 {
        return Err(yasna::ASN1Error::new(yasna::ASN1ErrorKind::Invalid));
    }
    r.read_der()
}

/// Whether `plain` is a PKCS#8 `PrivateKeyInfo` (see [`pkcs8_private_key_info`]).
#[cfg(test)]
fn is_pkcs8_private_key_info(plain: &[u8]) -> bool {
    pkcs8_private_key_info(plain).is_some()
}

/// The `(AlgorithmIdentifier, key bytes)` a SunPKCS12 SecretBag's plaintext
/// carries: `SEQUENCE { INTEGER 0, AlgorithmIdentifier, OCTET STRING key }`.
/// The acceptor [`open_pkcs12_envelope`] uses for secret keys.
fn secret_key_info(plain: &[u8]) -> Option<(p12::AlgorithmIdentifier, Vec<u8>)> {
    yasna::parse_ber(plain, |r| {
        r.read_sequence(|r| {
            let _version = r.next().read_u8()?;
            // NOT discarded (it used to be): this identifier is the only
            // record of the key's JCA algorithm name -- see
            // `EntryKind::SecretKey`.
            let algorithm = p12::AlgorithmIdentifier::parse(r.next())?;
            let key_bytes = r.next().read_bytes()?;
            Ok((algorithm, key_bytes))
        })
    })
    .ok()
}

/// Extend a PKCS#12 key entry's cert chain past its leaf by X.509 issuer/
/// subject matching: repeatedly take the last cert in `chain`, look for
/// another loaded cert (in either `certs_by_local_id` or `orphan_certs`)
/// whose subject DN equals that cert's issuer DN, and append it. Stops when
/// no match is found, the last cert is self-signed (a root), or after a
/// generous hop cap (real chains are a handful of certs deep; this only
/// guards against a malformed/cyclic file spinning forever). Matched certs
/// are removed from their source pool so they end up in exactly one chain,
/// not also flushed later as a standalone `TrustedCert` alias.
fn extend_chain_by_issuer(
    chain: &mut Vec<Vec<u8>>,
    certs_by_local_id: &mut IndexMap<Vec<u8>, Vec<(Option<String>, Vec<u8>)>>,
    orphan_certs: &mut Vec<(Option<String>, Vec<u8>)>,
) {
    const MAX_HOPS: usize = 16;
    for _ in 0..MAX_HOPS {
        let Some(last) = chain.last() else { break };
        let Ok(last_parsed) = crate::x509_manager::parse_certificate(last) else {
            break;
        };
        if last_parsed.issuer_der == last_parsed.subject_der {
            break; // self-signed root; nothing more to append.
        }

        // Search orphan_certs first (the common case: a shared/reused CA
        // cert with no localKeyId of its own), then any remaining
        // local-id-keyed groups.
        let mut found: Option<Vec<u8>> = None;
        if let Some(pos) = orphan_certs.iter().position(|(_, der)| {
            crate::x509_manager::parse_certificate(der)
                .map(|p| p.subject_der == last_parsed.issuer_der)
                .unwrap_or(false)
        }) {
            found = Some(orphan_certs.remove(pos).1);
        } else {
            'outer: for group in certs_by_local_id.values_mut() {
                if let Some(pos) = group.iter().position(|(_, der)| {
                    crate::x509_manager::parse_certificate(der)
                        .map(|p| p.subject_der == last_parsed.issuer_der)
                        .unwrap_or(false)
                }) {
                    found = Some(group.remove(pos).1);
                    break 'outer;
                }
            }
        }

        match found {
            Some(der) => chain.push(der),
            None => break,
        }
    }
    certs_by_local_id.retain(|_, group| !group.is_empty());
}

// ---------------------------------------------------------------------------
// BER -> DER normalisation
// ---------------------------------------------------------------------------
//
// PKCS#12 is a **BER** format, not a DER one, and the `p12` crate this module
// parses with accepts only DER. That is not a theoretical gap: BouncyCastle
// writes indefinite-length constructions, so every PKCS#12 file written by the
// most widely deployed third-party JCA provider was unreadable here.
//
// `openssl asn1parse` on the two, same certificate, same password:
//
// ```text
// BouncyCastle            JDK
//   0:d=0 hl=2 l=inf        0:d=0 hl=4 l= 786   SEQUENCE
//  20:d=3 hl=2 l=inf       26:d=3 hl=4 l= 681   OCTET STRING  (BC's is CONSTRUCTED)
// 669:d=4 hl=2 l=  0                            EOC
// ```
//
// Two BER features are in play and both are handled below:
//
// 1. **Indefinite lengths** - `80` in place of the length, terminated by an
//    end-of-contents `00 00`. Rewritten to the definite form.
// 2. **Segmented strings** - a CONSTRUCTED OCTET STRING whose children are the
//    pieces of one string. DER requires the primitive form, so the pieces are
//    concatenated. This matters beyond parsing: the PKCS#12 MAC is computed
//    over the *contents* of the authSafe OCTET STRING, which is exactly that
//    concatenation.
//
// This is deliberately a LENGTH normalisation and not a general BER-to-DER
// canonicaliser: SET OF ordering and primitive-value canonicalisation are left
// alone, because the parser does not depend on them and rewriting them would
// change bytes the MAC is taken over.

/// The BER indefinite-length marker.
const BER_INDEFINITE: u8 = 0x80;

/// Bound on nesting, so a corrupt or hostile file cannot recurse without end.
const BER_MAX_DEPTH: usize = 64;

struct BerHeader<'a> {
    /// The identifier octets, re-emitted verbatim.
    ident: &'a [u8],
    constructed: bool,
    /// `None` is the indefinite form.
    len: Option<usize>,
}

fn ber_header<'a>(src: &'a [u8], pos: &mut usize) -> Result<BerHeader<'a>, String> {
    let ident_start = *pos;
    let first = *src
        .get(*pos)
        .ok_or_else(|| "truncated identifier".to_string())?;
    *pos += 1;
    if first & 0x1f == 0x1f {
        // High-tag-number form: continues while the top bit is set.
        loop {
            let b = *src
                .get(*pos)
                .ok_or_else(|| "truncated high-tag-number identifier".to_string())?;
            *pos += 1;
            if b & 0x80 == 0 {
                break;
            }
        }
    }
    let ident = &src[ident_start..*pos];
    let l0 = *src
        .get(*pos)
        .ok_or_else(|| "truncated length".to_string())?;
    *pos += 1;
    let len = if l0 == BER_INDEFINITE {
        None
    } else if l0 & 0x80 == 0 {
        Some(l0 as usize)
    } else {
        let n = (l0 & 0x7f) as usize;
        if n == 0 || n > 8 {
            return Err(format!("unsupported long-form length 0x{l0:02x}"));
        }
        let mut v: usize = 0;
        for _ in 0..n {
            let b = *src
                .get(*pos)
                .ok_or_else(|| "truncated long-form length".to_string())?;
            *pos += 1;
            v = v
                .checked_mul(256)
                .and_then(|x| x.checked_add(b as usize))
                .ok_or_else(|| "length overflows usize".to_string())?;
        }
        Some(v)
    };
    Ok(BerHeader {
        ident,
        constructed: first & 0x20 != 0,
        len,
    })
}

/// Append `len` in the DER definite form (shortest encoding).
fn der_length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }
    let mut be = Vec::new();
    let mut v = len;
    while v > 0 {
        be.push((v & 0xff) as u8);
        v >>= 8;
    }
    be.reverse();
    out.push(0x80 | (be.len() as u8));
    out.extend_from_slice(&be);
}

/// A universal OCTET STRING (tag 4), primitive or constructed.
fn ber_is_octet_string(ident: &[u8]) -> bool {
    ident.len() == 1 && ident[0] & 0xc0 == 0x00 && ident[0] & 0x1f == 0x04
}

fn ber_rewrite_one(
    src: &[u8],
    pos: &mut usize,
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<(), String> {
    if depth > BER_MAX_DEPTH {
        return Err(format!("nesting deeper than {BER_MAX_DEPTH}"));
    }
    let header = ber_header(src, pos)?;
    let is_octet_string = ber_is_octet_string(header.ident);

    if !header.constructed {
        let len = header
            .len
            .ok_or_else(|| "primitive value with indefinite length".to_string())?;
        let end = pos
            .checked_add(len)
            .ok_or_else(|| "content length overflows".to_string())?;
        let content = src
            .get(*pos..end)
            .ok_or_else(|| "truncated primitive content".to_string())?;
        out.extend_from_slice(header.ident);
        der_length(len, out);
        out.extend_from_slice(content);
        *pos = end;
        return Ok(());
    }

    // Constructed: rewrite the children first, so the length emitted is the
    // length of the REWRITTEN body rather than the original one.
    let mut inner = Vec::new();
    match header.len {
        Some(len) => {
            let end = pos
                .checked_add(len)
                .ok_or_else(|| "content length overflows".to_string())?;
            if end > src.len() {
                return Err("truncated constructed content".to_string());
            }
            while *pos < end {
                ber_rewrite_one(src, pos, &mut inner, depth + 1)?;
            }
            if *pos != end {
                return Err("child value overran its parent".to_string());
            }
        }
        None => loop {
            let a = *src
                .get(*pos)
                .ok_or_else(|| "truncated indefinite-length value".to_string())?;
            let b = *src
                .get(*pos + 1)
                .ok_or_else(|| "truncated end-of-contents".to_string())?;
            if a == 0x00 && b == 0x00 {
                *pos += 2;
                break;
            }
            ber_rewrite_one(src, pos, &mut inner, depth + 1)?;
        },
    }

    if is_octet_string {
        // Segments of one string: DER wants them joined and primitive. Every
        // child is primitive by now, because a nested constructed OCTET STRING
        // took this same branch.
        let mut joined = Vec::new();
        let mut p = 0usize;
        while p < inner.len() {
            let child = ber_header(&inner, &mut p)?;
            let len = child
                .len
                .ok_or_else(|| "rewritten segment still indefinite".to_string())?;
            let end = p
                .checked_add(len)
                .ok_or_else(|| "segment length overflows".to_string())?;
            joined.extend_from_slice(
                inner
                    .get(p..end)
                    .ok_or_else(|| "truncated segment".to_string())?,
            );
            p = end;
        }
        out.push(header.ident[0] & !0x20);
        der_length(joined.len(), out);
        out.extend_from_slice(&joined);
    } else {
        out.extend_from_slice(header.ident);
        der_length(inner.len(), out);
        out.extend_from_slice(&inner);
    }
    Ok(())
}

/// Rewrite one BER value into the equivalent definite-length encoding.
///
/// A file that is already DER comes back byte-identical, which is what makes
/// this safe as a fallback: the DER path is unchanged.
pub fn ber_to_definite_length(src: &[u8]) -> Result<Vec<u8>, String> {
    let mut pos = 0usize;
    let mut out = Vec::with_capacity(src.len());
    ber_rewrite_one(src, &mut pos, &mut out, 0)?;
    Ok(out)
}

pub fn load_pkcs12(bytes: &[u8], password: &[u8]) -> Result<LoadedKeyStore, KeyStoreError> {
    load_pkcs12_ex(bytes, password, true)
}

/// See [`load_keystore_ex`] — `verify_mac=false` is how a Java `null`
/// password (`KeyStore.load(stream, null)`) reaches this parser. Real-JDK's
/// `PKCS12KeyStore.engineLoad` skips MAC verification entirely in that case
/// ("If a password is not given for integrity checking, then integrity
/// checking is not performed"); treating null the same as an empty `char[]`
/// would instead verify against an empty password and reject every
/// legitimately-passworded store loaded without a keystore password (e.g. a
/// PKCS#12 keystore opened only to read its key entries, whose password is
/// supplied later through `getKey()`).
pub fn load_pkcs12_ex(
    bytes: &[u8],
    password: &[u8],
    verify_mac: bool,
) -> Result<LoadedKeyStore, KeyStoreError> {
    // DER first, so a conforming file takes exactly the path it always did.
    // A BER one (BouncyCastle writes indefinite lengths and segmented OCTET
    // STRINGs) is normalised and retried - see `ber_to_definite_length`. Both
    // errors are reported if the retry also fails, because "the file is BER"
    // and "the file is corrupt" are different answers.
    let normalised: Vec<u8>;
    let pfx = match p12::PFX::parse(bytes) {
        Ok(pfx) => pfx,
        Err(der_err) => {
            normalised = ber_to_definite_length(bytes).map_err(|ber_err| {
                KeyStoreError::Pkcs12Parse(format!("{der_err:?}; not valid BER either: {ber_err}"))
            })?;
            p12::PFX::parse(&normalised).map_err(|e| {
                KeyStoreError::Pkcs12Parse(format!("{e:?} (after BER normalisation)"))
            })?
        }
    };

    // p12 takes the password as &str (it internally converts to UTF-16BE for
    // PBE-key derivation, matching the PKCS#12 spec). We ask the caller for
    // raw bytes so JKS can use them directly; for PKCS#12 we have to be a
    // valid &str. Anything that came through `char[]` is by definition a
    // valid UTF-16 sequence, so this conversion is lossless for any password
    // a Java caller could possibly produce.
    let password_str = std::str::from_utf8(password)
        .map_err(|_| KeyStoreError::Pkcs12Parse("password not UTF-8".into()))?;
    let password_bmp = pkcs12_bmp_string(password_str);

    // MAC verify (if a MAC is present) before we trust any decrypted bag.
    // Empty passwords MUST verify against an empty input the same way real
    // PKCS12KeyStore does; a Java-null password skips the check altogether.
    if verify_mac && !verify_pkcs12_mac(&pfx, password_str) {
        return Err(KeyStoreError::Pkcs12MacFailed);
    }

    // The `p12` crate parses every layer with yasna::parse_der (strict DER),
    // which rejects the JDK's per-bag attribute SET because the JDK does not
    // DER-sort it (friendlyName is written before the Oracle trustedKeyUsage
    // attribute, but encodes as a larger element so it sorts last). SunJSSE
    // reads it leniently, so we re-implement PFX::bags in BER mode, which
    // relaxes the SET-OF ordering check without skipping any structural
    // validation (kcfull #12).
    let bags = bags_ber(&pfx, password_str, !verify_mac)
        .map_err(|e| KeyStoreError::Pkcs12Parse(format!("bags(): {e:?}")))?;

    // Index bags by `localKeyId` so we can pair a private-key bag with the
    // matching cert chain. Real-JDK uses the same `localKeyId` attribute.
    // `IndexMap`, not `HashMap`: preserves the order bags were encountered in
    // the file, which `entries`'s assembly below relies on to match real
    // JDK's `LinkedHashMap`-backed alias enumeration order (see
    // `LoadedKeyStore::entries`'s doc comment).
    let mut keys_by_local_id: IndexMap<Vec<u8>, (Option<String>, Vec<u8>)> = IndexMap::new();
    let mut certs_by_local_id: IndexMap<Vec<u8>, Vec<(Option<String>, Vec<u8>)>> = IndexMap::new();
    let mut orphan_certs: Vec<(Option<String>, Vec<u8>)> = Vec::new();
    // `Option<String>` for the alias: a bag with no `friendlyName` is numbered
    // from the load-wide counter below, not named here.
    let mut secret_keys: Vec<(Option<String>, Vec<u8>, String)> = Vec::new();

    for bag in &bags {
        let friendly = bag.friendly_name();
        let local_id = bag.local_key_id().unwrap_or_default();

        match &bag.bag {
            p12::SafeBagKind::Pkcs8ShroudedKeyBag(epk) => {
                // gc-common w33-c: a plaintext counts only if it IS a PKCS#8
                // key. The ciphers are unauthenticated, so a store password
                // that is not the entry's used to "open" about one bag in 128
                // to noise with valid padding, and that noise replaced the
                // envelope `getKey`'s entry password had to open -- see
                // `open_pkcs12_envelope`.
                let opened =
                    open_pkcs12_envelope(epk, password, &password_bmp, pkcs8_private_key_info);
                // A PKCS#12 key bag may use a distinct entry password from the
                // store's own load/integrity password (SunPKCS12 only asks for
                // the entry password later, through `getKey()`). Real-JDK still
                // structurally registers the entry as a `PrivateKeyEntry` with
                // its full cert chain in that case -- `isKeyEntry()`/
                // `getCertificateChain()` work without ever decrypting the key,
                // and `getKey()` simply throws `UnrecoverableKeyException` later
                // if the password is wrong. Losing that structure here (by
                // dropping the bag out of `keys_by_local_id` entirely) broke
                // `SslInfo`/`SslMeterBinder`'s chain enumeration: the leaf cert
                // and its issuer chain fell through to the loose-cert-bags flush
                // below and got split into one bogus alias per certificate
                // instead of one `PrivateKeyEntry` alias with an N-cert chain.
                // Mirror the JKS loader's `jks_recover_key(...).unwrap_or(enc_key)`
                // pattern: keep the entry, just with the still-encrypted DER as
                // a placeholder key_der (re-serialized via `EncryptedPrivateKeyInfo
                // ::write`), so alias/chain pairing proceeds identically to a
                // successful decrypt.
                let key_der = opened.unwrap_or_else(|| {
                    tracing::debug!(
                        target: "keystore",
                        "deferring separately protected PKCS#12 key bag (kept as encrypted placeholder)"
                    );
                    yasna::construct_der(|w| epk.write(w))
                });
                keys_by_local_id
                    .entry(local_id.clone())
                    .or_insert((friendly, key_der));
            }
            p12::SafeBagKind::CertBag(p12::CertBag::X509(der)) => {
                if local_id.is_empty() {
                    orphan_certs.push((friendly, der.clone()));
                } else {
                    certs_by_local_id
                        .entry(local_id.clone())
                        .or_default()
                        .push((friendly, der.clone()));
                }
            }
            p12::SafeBagKind::OtherBagKind(other) => {
                // SunPKCS12 stores `SecretKeyEntry` values as a SecretBag
                // wrapping an EncryptedPrivateKeyInfo.  p12 0.6 exposes that
                // bag as an opaque OtherBag, so decode its standard inner
                // structure here and retain the encoded secret-key bytes.
                // Some producers encode the encrypted record directly;
                // SunPKCS12 retains the SecretBag sequence and wraps that
                // record in the `[0]` OCTET STRING payload.
                let epki_der =
                    if yasna::parse_ber(&other.bag_value, p12::EncryptedPrivateKeyInfo::parse)
                        .is_ok()
                    {
                        Some(other.bag_value.clone())
                    } else {
                        yasna::parse_ber(&other.bag_value, |r| {
                            r.read_sequence(|r| {
                                let _secret_type = r.next().read_oid()?;
                                r.next()
                                    .read_tagged(yasna::Tag::context(0), |r| r.read_bytes())
                            })
                        })
                        .ok()
                    };
                // gc-common w33-c: each decryption is judged by whether its
                // plaintext parses (`secret_key_info`), not only the first one
                // whose padding fitted -- a legacy-PBE false open used to hide
                // the PBES2 attempt that would have succeeded.
                let secret = epki_der.as_deref().and_then(|epki_der| {
                    let encrypted =
                        yasna::parse_ber(epki_der, p12::EncryptedPrivateKeyInfo::parse).ok()?;
                    open_pkcs12_envelope(&encrypted, password, &password_bmp, secret_key_info)
                });
                if let Some((algorithm, key_bytes)) = secret {
                    // `friendly` stays an `Option` here: an un-named bag's alias
                    // is assigned below, from the load-wide counter that has to
                    // number every entry kind in one sequence.
                    secret_keys.push((friendly, key_bytes, secret_alg_name(&algorithm)));
                }
            }
            _ => {}
        }
    }

    // Aliases for bags that carry NO `friendlyName`.
    //
    // The old rule here was "hex of the localKeyId, like keytool does". That is
    // what keytool PRINTS, and it is not what `sun.security.pkcs12.PKCS12KeyStore`
    // — the reader every `KeyStore.getInstance("PKCS12")` actually uses — does.
    // Its `getUnfriendlyName()` is `counter++; return String.valueOf(counter)`,
    // so an un-named bag is called "1", "2", … in bag order, and the counter is
    // shared by every entry kind in one load.
    //
    // Measured on jdk-25.0.3.9-hotspot against netty's own
    // `mutual_auth_server.p12` (a `localKeyID` and no `friendlyName`): HotSpot
    // reports alias `1`; CratonVM reported
    // `70e4faffe3f82e2db1c949282bf86a7c17348838`. Any caller that looks an entry
    // up by the alias its own test data documents then missed —
    // `OpenSslKeyMaterialProviderTest` asks for `"1"` and got null key material
    // (found while closing the retired `ssl-suite-test-discovery-undercounts`
    // write-up, whose fix made the OpenSSL half of those classes run at all).
    let mut unfriendly_counter: u32 = 0;

    let mut entries: IndexMap<String, KeyStoreEntry> = IndexMap::new();

    for (friendly, key_bytes, algorithm) in secret_keys {
        let alias = friendly.unwrap_or_else(|| unfriendly_alias(&mut unfriendly_counter));
        entries.insert(
            alias.clone(),
            KeyStoreEntry {
                alias,
                creation_time_ms: 0,
                kind: EntryKind::SecretKey {
                    key_bytes,
                    algorithm,
                },
            },
        );
    }

    // Pair keys with their cert chains. Only the leaf cert typically shares
    // the key's `localKeyId` -- SunPKCS12 builds the REST of the chain by
    // repeatedly matching each cert's issuer DN against another loaded
    // cert's subject DN (real X.509 chain-building), not by `localKeyId`.
    // Without this, a multi-cert chain (leaf + intermediate + root) only
    // ever surfaced its leaf here, and the unclaimed issuer certs fell
    // through to the loose-cert-bag flush below as spurious standalone
    // aliases (named by their subject DN, e.g. "CN=ca") instead of being
    // part of this entry's chain -- see `SslMeterBinderTests`'s gauge-count
    // residual (module/spring-boot-micrometer-metrics).
    for (local_id, (key_friendly, key_der)) in keys_by_local_id {
        let mut chain: Vec<Vec<u8>> = Vec::new();
        let mut chain_friendly: Option<String> = None;
        if let Some(matched) = certs_by_local_id.shift_remove(&local_id) {
            for (fn_, der) in matched {
                if chain_friendly.is_none() && fn_.is_some() {
                    chain_friendly = fn_;
                }
                chain.push(der);
            }
        }
        extend_chain_by_issuer(&mut chain, &mut certs_by_local_id, &mut orphan_certs);

        let _ = &local_id;
        let alias = key_friendly
            .or(chain_friendly)
            .unwrap_or_else(|| unfriendly_alias(&mut unfriendly_counter));

        entries.insert(
            alias.clone(),
            KeyStoreEntry {
                alias,
                creation_time_ms: 0,
                kind: EntryKind::PrivateKey {
                    key_der,
                    chain,
                    protected: None,
                },
            },
        );
    }

    // Any cert-bags that didn't pair with a key go in as TrustedCert entries.
    let mut walk = certs_by_local_id
        .into_iter()
        .flat_map(|(_, v)| v)
        .collect::<Vec<_>>();
    walk.extend(orphan_certs);
    for (idx, (friendly, der)) in walk.into_iter().enumerate() {
        let _ = idx;
        let alias = friendly.unwrap_or_else(|| unfriendly_alias(&mut unfriendly_counter));
        entries.insert(
            alias.clone(),
            KeyStoreEntry {
                alias,
                creation_time_ms: 0,
                kind: EntryKind::TrustedCert { cert_der: der },
            },
        );
    }

    Ok(LoadedKeyStore { entries })
}

// ---------------------------------------------------------------------------
// JKS — hand-rolled walker
// ---------------------------------------------------------------------------
//
// File layout:
//   u32 magic = 0xFEEDFEED
//   u32 version (1 or 2)
//   u32 entry_count
//   entry_count * {
//     u32 tag (1 = PrivateKeyEntry, 2 = TrustedCertEntry)
//     u16 alias_len + UTF-8 alias  (NB: real JKS uses Java's "modified UTF-8";
//                                   for ASCII aliases the two are identical)
//     u64 creation_date_ms
//     match tag {
//       1 => {
//         u32 enc_key_len + enc_key_bytes (proprietary "JKS encryption" wrapper
//                                          around a PKCS#8 private-key DER —
//                                          we keep it as-is, the receiver of
//                                          the bytes knows the format)
//         u32 chain_count
//         chain_count * {
//           u16 cert_type_len + cert_type ("X.509")
//           u32 cert_der_len + cert_der
//         }
//       }
//       2 => {
//         u16 cert_type_len + cert_type
//         u32 cert_der_len + cert_der
//       }
//     }
//   }
//   [SHA1(password_utf16be || "Mighty Aphrodite" || body)]  // 20 bytes

pub fn load_jks(bytes: &[u8], password: &[u8]) -> Result<LoadedKeyStore, KeyStoreError> {
    if bytes.len() < 4 + 4 + 4 + 20 {
        return Err(KeyStoreError::Truncated(0));
    }

    // Verify the trailing 20-byte SHA-1 integrity tag FIRST, before any
    // structural parsing. Any single-byte flip in the body (e.g. corrupt
    // entry-count) must be rejected as JksMacMismatch — not as a downstream
    // Truncated error from the parser walking off the end of the buffer.
    // RFC: JKS HMAC covers `(password as UTF-16BE) || "Mighty Aphrodite"
    // || body`, where body is everything before the final 20 bytes.
    let body_end = bytes.len() - 20;
    // Real-JDK JavaKeyStore only verifies the integrity HMAC when a password is
    // supplied; a null/empty password loads the certs without the check (the
    // standard way to read a truststore). Mirror that — otherwise loading e.g. a
    // JSSE truststore with no password failed with the `JksMacMismatch` message
    // below and broke SSLContext creation.
    if !password.is_empty() {
        let stored_mac = &bytes[body_end..];
        let body = &bytes[..body_end];
        let computed = jks_password_mac(password, body);
        if !constant_time_eq(stored_mac, &computed) {
            return Err(KeyStoreError::JksMacMismatch);
        }
    }

    let mut r = JksReader::new(bytes);
    let magic = r.u32_be()?;
    // Either magic: the record layout that follows is identical, and refusing
    // JCEKS here would make the type registered above unreadable by its own
    // writer.
    if magic != JKS_MAGIC && magic != JCEKS_MAGIC {
        return Err(KeyStoreError::BadJksMagic);
    }
    let version = r.u32_be()?;
    if version != 1 && version != 2 {
        return Err(KeyStoreError::BadJksVersion(version));
    }
    let entry_count = r.u32_be()? as usize;

    let mut entries: IndexMap<String, KeyStoreEntry> = IndexMap::new();

    for _ in 0..entry_count {
        let tag = r.u32_be()?;
        let alias = r.utf8_u16len()?;
        let creation_time_ms = r.u64_be()? as i64;

        match tag {
            1 => {
                // PrivateKeyEntry. The stored bytes are the JKS-protected key
                // (an EncryptedPrivateKeyInfo); decrypt it to plaintext PKCS#8
                // via the JKS KeyProtector so downstream consumers (rustls TLS)
                // get a parseable key. If decryption fails (e.g. a per-key
                // password we don't have), keep the raw bytes rather than drop
                // the entry — callers that don't need the key still see it.
                let enc_key = r.bytes_u32len()?;
                let key_der = jks_recover_key(&enc_key, password).unwrap_or(enc_key);
                let chain_count = r.u32_be()? as usize;
                // SECURITY: `chain_count` is attacker-controlled. A forged
                // truststore (especially one loaded with an empty password,
                // which skips the integrity MAC) could declare e.g.
                // 0xFFFFFFFF certs and force `Vec::with_capacity(4G)` → OOM
                // DoS before a single cert is parsed. Each chain element
                // costs at minimum a u16 cert-type length (2 bytes) + a u32
                // cert-der length (4 bytes) = 6 bytes in the stream, so a
                // count larger than `remaining / 6` is structurally
                // impossible — reject it as truncated rather than trusting it.
                // We also cap the *reserved* capacity (not the loop bound) at
                // the same realistic ceiling so we never pre-reserve for more
                // certs than the buffer can physically contain, and let the
                // per-element `need()` checks in the loop do the final
                // enforcement (Vec grows incrementally via `push`).
                const JKS_CHAIN_MIN_ELEM_BYTES: usize = 6; // u16 type-len + u32 der-len
                let max_possible_chain = r.remaining() / JKS_CHAIN_MIN_ELEM_BYTES;
                if chain_count > max_possible_chain {
                    return Err(KeyStoreError::Truncated(r.pos()));
                }
                let mut chain = Vec::with_capacity(chain_count.min(max_possible_chain));
                for _ in 0..chain_count {
                    let _cert_type = r.utf8_u16len()?;
                    let cert_der = r.bytes_u32len()?;
                    chain.push(cert_der);
                }
                entries.insert(
                    alias.clone(),
                    KeyStoreEntry {
                        alias,
                        creation_time_ms,
                        kind: EntryKind::PrivateKey {
                            key_der,
                            chain,
                            protected: None,
                        },
                    },
                );
            }
            2 => {
                // TrustedCertEntry
                if version == 2 {
                    let _cert_type = r.utf8_u16len()?;
                }
                // v1 stores cert directly; v2 wraps with cert_type prefix.
                // For v1 we still read the (cert_type) prefix because real
                // OpenJDK source does so unconditionally — the version
                // distinction here is for *tag-3* (sealed) entries which
                // we don't support. Keep one path:
                if version == 1 {
                    let _cert_type = r.utf8_u16len()?;
                }
                let cert_der = r.bytes_u32len()?;
                entries.insert(
                    alias.clone(),
                    KeyStoreEntry {
                        alias,
                        creation_time_ms,
                        kind: EntryKind::TrustedCert { cert_der },
                    },
                );
            }
            other => return Err(KeyStoreError::BadJksTag(other)),
        }
    }

    // HMAC was already verified at the top of this function, so any
    // structural parse that reached here is trustworthy. Defense-in-depth
    // sanity check: parser must have consumed exactly `body_end` bytes
    // (i.e. body length declared by the HMAC envelope must match the
    // entries we parsed). Mismatch here means the body contains trailing
    // padding/garbage despite a valid HMAC — reject as malformed.
    if r.pos() != body_end {
        return Err(KeyStoreError::Truncated(r.pos()));
    }
    Ok(LoadedKeyStore { entries })
}

/// PBES2 / PBKDF2-HMAC-SHA256 / AES-256-CBC encryption of one PKCS#8-shaped
/// blob, producing the `EncryptedPrivateKeyInfo` a PKCS#12 shrouded key bag
/// or secret bag carries.
///
/// This is the exact inverse of [`decrypt_pbes2_params`] above, and the same
/// envelope current `SunPKCS12` writes by default
/// (`keystore.pkcs12.keyProtectionAlgorithm` = `PBEWithHMACSHA256AndAES_256`),
/// so the output is readable by both this module's loader and a real JDK.
///
/// Returns `None` when OS entropy is unavailable -- a predictable salt/IV here
/// would be worse than refusing, and [`write_pkcs12`] turns the `None` into a
/// visible error rather than an unprotected or silently-dropped key.
fn pbes2_encrypt(plain: &[u8], password: &[u8]) -> Option<p12::EncryptedPrivateKeyInfo> {
    // 10_000 rounds is SunPKCS12's own default for this algorithm; matching it
    // keeps a CV-written keystore indistinguishable from a keytool-written one
    // in the one parameter a reader is entitled to sanity-check.
    const ITERATIONS: u32 = 10_000;

    let mut salt = [0u8; 20];
    let mut iv = [0u8; 16];
    if !crate::securerandom::os_random_bytes(&mut salt) {
        return None;
    }
    if !crate::securerandom::os_random_bytes(&mut iv) {
        return None;
    }
    pbes2_encrypt_with(plain, password, &salt, &iv, ITERATIONS)
}

/// [`pbes2_encrypt`] with the salt, IV and iteration count supplied, so a test
/// can build a chosen envelope (gc-common w33-c) -- a PBES2 envelope is
/// random by construction, and the wrong-password false opens
/// [`open_pkcs12_envelope`] now refuses happen for one salt/IV in ~128.
fn pbes2_encrypt_with(
    plain: &[u8],
    password: &[u8],
    salt: &[u8; 20],
    iv: &[u8; 16],
    iterations: u32,
) -> Option<p12::EncryptedPrivateKeyInfo> {
    use aes::cipher::block_padding::Pkcs7;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncryptMut, KeyIvInit};

    const KEY_LEN: usize = 32;
    let (salt, iv) = (*salt, *iv);

    // prf 256 == HMAC-SHA256, the encoding `decrypt_pbes2_params` uses.
    let key = crate::phases_early::pbkdf2_derive_for(256, password, &salt, iterations, KEY_LEN);
    let mut out = vec![0u8; plain.len() + 16];
    let written = cbc::Encryptor::<aes::Aes256>::new(
        GenericArray::from_slice(&key),
        GenericArray::from_slice(&iv),
    )
    .encrypt_padded_b2b_mut::<Pkcs7>(plain, &mut out)
    .ok()?
    .len();
    out.truncate(written);

    let params = yasna::construct_der(|w| {
        w.write_sequence(|w| {
            // keyDerivationFunc: PBKDF2 { salt, iterationCount, keyLength, prf }
            w.next().write_sequence(|w| {
                w.next()
                    .write_oid(&yasna::models::ObjectIdentifier::from_slice(&[
                        1, 2, 840, 113549, 1, 5, 12,
                    ]));
                w.next().write_sequence(|w| {
                    w.next().write_bytes(&salt);
                    w.next().write_u32(iterations);
                    w.next().write_u32(KEY_LEN as u32);
                    w.next().write_sequence(|w| {
                        w.next()
                            .write_oid(&yasna::models::ObjectIdentifier::from_slice(&[
                                1, 2, 840, 113549, 2, 9,
                            ]));
                        w.next().write_null();
                    });
                });
            });
            // encryptionScheme: aes256-CBC { iv }
            w.next().write_sequence(|w| {
                w.next()
                    .write_oid(&yasna::models::ObjectIdentifier::from_slice(&[
                        2, 16, 840, 1, 101, 3, 4, 1, 42,
                    ]));
                w.next().write_bytes(&iv);
            });
        })
    });

    Some(p12::EncryptedPrivateKeyInfo {
        encryption_algorithm: p12::AlgorithmIdentifier::OtherAlg(p12::OtherAlgorithmIdentifier {
            // PBES2
            algorithm_type: yasna::models::ObjectIdentifier::from_slice(&[
                1, 2, 840, 113549, 1, 5, 13,
            ]),
            params: Some(params),
        }),
        encrypted_data: out,
    })
}

/// Serialise a keystore to PKCS#12 (RFC 7292).
///
/// WHY THIS EXISTS AT ALL: JKS -- what [`write_jks`] emits, and what
/// `engineStore` wrote for every store regardless of declared type -- has no
/// representation for a `SecretKeyEntry`. A keystore holding one therefore had
/// to either change format or lose the entry, and it silently lost it: the
/// alias was filtered out of the JKS body and the caller got a structurally
/// valid, entry-less file plus no exception
/// (`docs/feature-designs/native-dispatch-architecture.md`).
///
/// Shape, and why each choice: one unencrypted `SafeContents`
/// (`ContentInfo::Data`) carrying every bag; private keys in a
/// `pkcs8ShroudedKeyBag` and secret keys in a `secretBag`, both wrapped in the
/// PBES2 envelope [`pbes2_encrypt`] builds; certificates in the clear inside
/// that same SafeContents. `SunPKCS12` additionally encrypts the certificate
/// SafeContents -- nothing requires it, certificates are public, and leaving
/// them readable is what keeps a CV-written store listable by a plain
/// `keytool -list` with no password. Integrity is a SHA-256 `MacData` over the
/// AuthenticatedSafe, which is what [`verify_pkcs12_mac`] checks on the way
/// back in.
///
/// Cert-bag attributes mirror `SunPKCS12` exactly: the LEAF of a key entry's
/// chain carries `localKeyId` + `friendlyName`, the rest of the chain carries
/// nothing and is re-attached on load by issuer/subject matching
/// ([`extend_chain_by_issuer`]). Giving every chain cert the same `localKeyId`
/// would read back correctly here but is not what a real JDK expects to find.
///
/// KNOWN LIMITATION, stated rather than hidden: PKCS#12 permits a per-entry
/// key password distinct from the store password, and a `LoadedKeyStore` does
/// not carry one -- every key is protected with the STORE password, exactly
/// like [`jks_protect_key`] does for JKS.
pub fn write_pkcs12(store: &LoadedKeyStore, password: &[u8]) -> Result<Vec<u8>, String> {
    use yasna::models::ObjectIdentifier;

    let oid_pkcs8_shrouded = ObjectIdentifier::from_slice(&[1, 2, 840, 113549, 1, 12, 10, 1, 2]);
    let oid_secret_bag = ObjectIdentifier::from_slice(&[1, 2, 840, 113549, 1, 12, 10, 1, 5]);

    let mut bags: Vec<p12::SafeBag> = Vec::new();
    let mut next_key_id: u32 = 0;

    for (alias, entry) in store.entries.iter() {
        match &entry.kind {
            EntryKind::TrustedCert { cert_der } => {
                // The `trustedKeyUsage` attribute is what makes this a TRUSTED
                // certificate entry rather than a stray certificate. Without
                // it, a real JDK reading the file drops the bag entirely (it
                // keeps only certs that pair with a key), which is how the
                // first cut of this writer produced a 4-entry keystore that
                // HotSpot listed as 3 — silent loss of exactly the kind this
                // record is about. `AnyUsage` (2.5.29.37.0) is the value
                // `PKCS12KeyStore.setCertEntry` writes.
                bags.push(p12::SafeBag {
                    bag: p12::SafeBagKind::CertBag(p12::CertBag::X509(cert_der.clone())),
                    attributes: vec![
                        p12::PKCS12Attribute::FriendlyName(alias.clone()),
                        p12::PKCS12Attribute::Other(p12::OtherAttribute {
                            oid: ObjectIdentifier::from_slice(&[
                                2, 16, 840, 1, 113894, 746875, 1, 1,
                            ]),
                            data: vec![yasna::construct_der(|w| {
                                w.write_oid(&ObjectIdentifier::from_slice(&[2, 5, 29, 37, 0]))
                            })],
                        }),
                    ],
                });
            }
            EntryKind::PrivateKey {
                key_der,
                chain,
                protected: entry_envelope,
            } => {
                next_key_id += 1;
                let key_id = next_key_id.to_be_bytes().to_vec();
                // KS-5: an entry set through the API carries the envelope its
                // OWN password opens. Emit that, rather than re-encrypting the
                // plaintext under the STORE password: before this the entry
                // password protected nothing on disk, and a file this VM wrote
                // could not be opened with it by keytool or any other JCA
                // reader -- MEASURED against HotSpot 25.0.3+9 on all three
                // store types.
                //
                // A key that never decrypted is still inside its ORIGINAL
                // envelope (see `load_jks`/`load_pkcs12`'s
                // `unwrap_or(encrypted)` arms). Re-wrapping it would
                // double-encrypt; pass it straight through when it already
                // parses as an EncryptedPrivateKeyInfo.
                //
                // Not a JKS `KeyProtector` envelope, though (gc-common w34-c):
                // since `keystore_unlock_private_keys` keeps the envelope it
                // opened, a JKS entry opened by `getKey` and then written out
                // as PKCS#12 (a `DualFormatPKCS12` that loaded a JKS file) would
                // otherwise put an algorithm no PKCS#12 reader knows into the
                // file. Such an entry is re-protected below, as before.
                let epki = if let Some(existing) = entry_envelope
                    .as_deref()
                    .filter(|e| !is_jks_encrypted_private_key(e))
                    .and_then(|e| yasna::parse_ber(e, p12::EncryptedPrivateKeyInfo::parse).ok())
                {
                    existing
                } else if let Ok(existing) =
                    yasna::parse_ber(key_der, p12::EncryptedPrivateKeyInfo::parse)
                {
                    existing
                } else {
                    pbes2_encrypt(key_der, password).ok_or_else(|| {
                        format!(
                            "write_pkcs12({alias:?}): OS entropy unavailable, refusing to \
                             protect a private key with a predictable salt"
                        )
                    })?
                };
                bags.push(p12::SafeBag {
                    bag: p12::SafeBagKind::Pkcs8ShroudedKeyBag(epki),
                    attributes: vec![
                        p12::PKCS12Attribute::FriendlyName(alias.clone()),
                        p12::PKCS12Attribute::LocalKeyId(key_id.clone()),
                    ],
                });
                for (idx, cert_der) in chain.iter().enumerate() {
                    bags.push(p12::SafeBag {
                        bag: p12::SafeBagKind::CertBag(p12::CertBag::X509(cert_der.clone())),
                        attributes: if idx == 0 {
                            vec![
                                p12::PKCS12Attribute::FriendlyName(alias.clone()),
                                p12::PKCS12Attribute::LocalKeyId(key_id.clone()),
                            ]
                        } else {
                            Vec::new()
                        },
                    });
                }
            }
            EntryKind::SecretKey {
                key_bytes,
                algorithm,
            } => {
                let Some(alg_oid) = secret_key_alg_oid(algorithm) else {
                    return Err(format!(
                        "write_pkcs12({alias:?}): no PKCS#12 OID is known for secret-key \
                         algorithm {algorithm:?}, and writing it under a wrong OID would \
                         read back as a different algorithm"
                    ));
                };
                // The PKCS#8-shaped plaintext SunPKCS12 encrypts:
                //   SEQUENCE { INTEGER 0, AlgorithmIdentifier, OCTET STRING key }
                let plain = yasna::construct_der(|w| {
                    w.write_sequence(|w| {
                        w.next().write_u8(0);
                        w.next().write_sequence(|w| {
                            w.next().write_oid(&alg_oid);
                        });
                        w.next().write_bytes(key_bytes);
                    })
                });
                let epki = pbes2_encrypt(&plain, password).ok_or_else(|| {
                    format!(
                        "write_pkcs12({alias:?}): OS entropy unavailable, refusing to protect \
                         a secret key with a predictable salt"
                    )
                })?;
                let epki_der = yasna::construct_der(|w| epki.write(w));
                // SecretBag ::= SEQUENCE { secretTypeId, [0] EXPLICIT ANY }.
                // SunPKCS12 writes pkcs8ShroudedKeyBag as the type id and the
                // EncryptedPrivateKeyInfo as an OCTET STRING inside the tag.
                let secret_bag = yasna::construct_der(|w| {
                    w.write_sequence(|w| {
                        w.next().write_oid(&oid_pkcs8_shrouded);
                        w.next()
                            .write_tagged(yasna::Tag::context(0), |w| w.write_bytes(&epki_der));
                    })
                });
                bags.push(p12::SafeBag {
                    bag: p12::SafeBagKind::OtherBagKind(p12::OtherBag {
                        bag_id: oid_secret_bag.clone(),
                        bag_value: secret_bag,
                    }),
                    attributes: vec![p12::PKCS12Attribute::FriendlyName(alias.clone())],
                });
            }
        }
    }

    let safe_contents = yasna::construct_der(|w| {
        w.write_sequence_of(|w| {
            for bag in &bags {
                bag.write(w.next());
            }
        })
    });
    let auth_safe = yasna::construct_der(|w| {
        w.write_sequence_of(|w| {
            p12::ContentInfo::Data(safe_contents.clone()).write(w.next());
        })
    });

    // MacData over the AuthenticatedSafe. SHA-256, because that is what
    // `pkcs12_mac_digest` names as the default current SunPKCS12 emits and
    // what `verify_pkcs12_mac` will re-derive on the way back in.
    const MAC_ITERATIONS: u32 = 10_000;
    let mut mac_salt = [0u8; 20];
    if !crate::securerandom::os_random_bytes(&mut mac_salt) {
        return Err(
            "write_pkcs12: OS entropy unavailable, refusing to emit a PKCS#12 MAC with a \
             predictable salt"
                .to_string(),
        );
    }
    let password_str = String::from_utf8_lossy(password).to_string();
    let password_bmp = pkcs12_bmp_string(&password_str);
    let digest = Pkcs12MacDigest::Sha256;
    let mac_key = pkcs12_mac_kdf(
        digest,
        &password_bmp,
        &mac_salt,
        MAC_ITERATIONS,
        3,
        digest.output_len(),
    );
    let mac_data = p12::MacData {
        mac: p12::DigestInfo {
            digest_algorithm: p12::AlgorithmIdentifier::OtherAlg(p12::OtherAlgorithmIdentifier {
                algorithm_type: ObjectIdentifier::from_slice(&[2, 16, 840, 1, 101, 3, 4, 2, 1]),
                params: Some(yasna::construct_der(|w| w.write_null())),
            }),
            digest: pkcs12_hmac(digest, &mac_key, &auth_safe),
        },
        salt: mac_salt.to_vec(),
        iterations: MAC_ITERATIONS,
    };

    let pfx = p12::PFX {
        version: 3,
        auth_safe: p12::ContentInfo::Data(auth_safe),
        mac_data: Some(mac_data),
    };
    Ok(pfx.to_der())
}

/// Serialise a keystore to the JKS v2 wire format (magic, version, entry
/// count, per-entry records, trailing `SHA1(pw||salt||body)` tag). Used by
/// `engineStore`: CV's read path (`load_keystore`) detects the format by magic,
/// so writing JKS round-trips through CV's own JKS parser regardless of the
/// `KeyStore` type the caller declared (the keycloak truststore round-trip
/// stores as "PKCS12" but is detected/loaded by content). Mirrors `load_jks`'s
/// record layout exactly. Trusted certs are written as tag-2 entries; private
/// keys as tag-1 wrapped in Sun's `KeyProtector` `EncryptedPrivateKeyInfo`
/// envelope (see `jks_protect_key`), which is the exact inverse of the
/// `jks_recover_key` call `load_jks` makes on the way back in.
pub fn write_jks(store: &LoadedKeyStore, password: &[u8]) -> Vec<u8> {
    write_jks_with_magic(store, password, JKS_MAGIC)
}

/// [`write_jks`] under an explicit magic, so JCEKS can share the record writer.
///
/// The two formats differ in the magic and in how a private key is protected;
/// this writer does not protect private keys differently per format, so the
/// magic is the whole difference it can express -- and writing a JCEKS store
/// under the JKS magic would produce a file `keytool -storetype JCEKS` refuses.
pub fn write_jks_with_magic(store: &LoadedKeyStore, password: &[u8], magic: u32) -> Vec<u8> {
    let cert_type: &[u8] = b"X.509";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&magic.to_be_bytes());
    body.extend_from_slice(&2u32.to_be_bytes()); // version 2
                                                 // JKS has no compatible representation for SecretKeyEntry.  Keep the
                                                 // entry available in memory, but omit it from this legacy wire format.
                                                 // (PKCS#12 callers are still loaded from their original SecretBag.)
    let aliases: Vec<&String> = store
        .entries
        .iter()
        .filter_map(|(alias, entry)| match &entry.kind {
            EntryKind::SecretKey { .. } => None,
            _ => Some(alias),
        })
        .collect();
    body.extend_from_slice(&(aliases.len() as u32).to_be_bytes());

    // INSERTION order, not alphabetical. `LoadedKeyStore::entries` is an
    // `IndexMap` precisely because real JDK enumerates a keystore in the order
    // its entries were read/inserted, and the `sort_unstable()` that used to
    // stand here made a store -> load round trip silently re-alphabetise them:
    // `setKeyEntry("pk", ..); setCertificateEntry("ca", ..)` came back as
    // `[ca, pk]` where HotSpot answers `[pk, ca]`. `IndexMap` iteration is
    // itself deterministic, which is all that sort was reaching for.
    for alias in aliases {
        let entry = &store.entries[alias];
        let ab = alias.as_bytes();
        match &entry.kind {
            EntryKind::TrustedCert { cert_der } => {
                body.extend_from_slice(&2u32.to_be_bytes()); // tag = TrustedCertEntry
                body.extend_from_slice(&(ab.len() as u16).to_be_bytes());
                body.extend_from_slice(ab);
                body.extend_from_slice(&(entry.creation_time_ms as u64).to_be_bytes());
                body.extend_from_slice(&(cert_type.len() as u16).to_be_bytes());
                body.extend_from_slice(cert_type);
                body.extend_from_slice(&(cert_der.len() as u32).to_be_bytes());
                body.extend_from_slice(cert_der);
            }
            EntryKind::PrivateKey {
                key_der,
                chain,
                protected: entry_envelope,
            } => {
                // Re-apply Sun's KeyProtector envelope. `load_jks` stores the
                // DECRYPTED PKCS#8 (see its tag-1 arm), so writing `key_der`
                // straight out — the previous behaviour — put an UNPROTECTED
                // private key on disk in a file the caller had just supplied a
                // password for, and produced a file no real JDK could read.
                // Two pass-through cases stay byte-identical to before:
                //   * an entry whose key never decrypted (wrong/absent key
                //     password) still holds the ORIGINAL envelope — re-wrapping
                //     it would double-encrypt;
                //   * an entropy failure, where refusing to write anything at
                //     all would lose the entry; that path keeps the old
                //     behaviour and warns loudly rather than emitting a
                //     predictable-salt envelope.
                let protected: Vec<u8> = if let Some(env) = entry_envelope
                    .as_deref()
                    .filter(|e| is_jks_encrypted_private_key(e))
                {
                    // KS-5: the envelope this entry's OWN password opens.
                    env.to_vec()
                } else if is_jks_encrypted_private_key(key_der) {
                    key_der.clone()
                } else {
                    match jks_protect_key(key_der, password) {
                        Some(wrapped) => wrapped,
                        None => {
                            tracing::warn!(
                                target: "keystore",
                                alias = %alias,
                                "OS entropy unavailable; JKS private key written UNPROTECTED"
                            );
                            key_der.clone()
                        }
                    }
                };
                body.extend_from_slice(&1u32.to_be_bytes()); // tag = PrivateKeyEntry
                body.extend_from_slice(&(ab.len() as u16).to_be_bytes());
                body.extend_from_slice(ab);
                body.extend_from_slice(&(entry.creation_time_ms as u64).to_be_bytes());
                body.extend_from_slice(&(protected.len() as u32).to_be_bytes());
                body.extend_from_slice(&protected);
                body.extend_from_slice(&(chain.len() as u32).to_be_bytes());
                for c in chain {
                    body.extend_from_slice(&(cert_type.len() as u16).to_be_bytes());
                    body.extend_from_slice(cert_type);
                    body.extend_from_slice(&(c.len() as u32).to_be_bytes());
                    body.extend_from_slice(c);
                }
            }
            EntryKind::SecretKey { .. } => unreachable!("secret keys are filtered above"),
        }
    }
    let mac = jks_password_mac(password, &body);
    body.extend_from_slice(&mac);
    body
}

/// JKS integrity tag: `SHA1(password_utf16be || "Mighty Aphrodite" || body)`.
///
/// This is *not* a standard HMAC. Sun/OpenJDK's `JavaKeyStore` invented this
/// construction in JDK 1.2 and it has been frozen since. Empty passwords
/// produce an empty UTF-16 prefix (the SHA-1 starts straight from the salt
/// + body).
fn jks_password_mac(password_bytes: &[u8], body: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    // Treat each input byte as a Latin-1 codepoint and emit UTF-16BE: the
    // high byte is 0, the low byte is the original. This matches what the
    // JDK does for the common ASCII case (Java `char[]` produced by
    // `password.toCharArray()` where each char is `<= 0x00FF`). For exotic
    // passwords callers would have to pass the bytes already in UTF-16BE
    // form; that path is rarely exercised.
    for b in password_bytes {
        hasher.update([0u8, *b]);
    }
    hasher.update(JKS_HMAC_SALT);
    hasher.update(body);
    let out = hasher.finalize();
    let mut tag = [0u8; 20];
    tag.copy_from_slice(&out);
    tag
}

/// Encode a password (bytes treated as Latin-1 codepoints) as UTF-16BE, the
/// form JKS feeds to SHA-1 (high byte 0, low byte original).
fn jks_passwd_utf16be(password_bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(password_bytes.len() * 2);
    for b in password_bytes {
        v.push(0u8);
        v.push(*b);
    }
    v
}

/// Minimal DER walker: read a length field at `pos`, returning (length, new_pos).
fn der_read_len(data: &[u8], mut pos: usize) -> Option<(usize, usize)> {
    let first = *data.get(pos)?;
    pos += 1;
    if first < 0x80 {
        return Some((first as usize, pos));
    }
    let n = (first & 0x7f) as usize;
    if n == 0 || n > 4 {
        return None;
    }
    let mut len = 0usize;
    for _ in 0..n {
        len = (len << 8) | (*data.get(pos)? as usize);
        pos += 1;
    }
    Some((len, pos))
}

/// Extract the `encryptedData` OCTET STRING from an `EncryptedPrivateKeyInfo`
/// DER: `SEQUENCE { AlgorithmIdentifier, OCTET STRING }`. Returns the octet
/// content (for JKS: `salt(20) || encryptedKey || digest(20)`).
fn der_extract_epki_octets(der: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    if *der.get(pos)? != 0x30 {
        return None;
    }
    pos += 1;
    let (_, p) = der_read_len(der, pos)?;
    pos = p;
    // AlgorithmIdentifier SEQUENCE — skip whole.
    if *der.get(pos)? != 0x30 {
        return None;
    }
    pos += 1;
    let (alg_len, p) = der_read_len(der, pos)?;
    pos = p + alg_len;
    // OCTET STRING.
    if *der.get(pos)? != 0x04 {
        return None;
    }
    pos += 1;
    let (oct_len, p) = der_read_len(der, pos)?;
    pos = p;
    der.get(pos..pos + oct_len).map(|s| s.to_vec())
}

/// Recover a JKS-protected private key into its plaintext PKCS#8 DER.
///
/// JKS wraps the PKCS#8 key in an `EncryptedPrivateKeyInfo` whose OCTET STRING
/// is `salt(20) || (plainPkcs8 XOR keystream) || SHA1(passwd||plainPkcs8)`,
/// where the keystream is `Wi = SHA1(passwdUtf16be || W(i-1))`, `W0 = salt`
/// (Sun's frozen JDK-1.2 `KeyProtector`). Returns the plaintext PKCS#8 DER, or
/// `None` if the structure / integrity check doesn't hold.
fn jks_recover_key(epki_der: &[u8], password_bytes: &[u8]) -> Option<Vec<u8>> {
    use sha1::{Digest, Sha1};
    let protected = der_extract_epki_octets(epki_der)?;
    if protected.len() < 40 {
        return None;
    }
    let salt = &protected[..20];
    let encr_len = protected.len() - 40;
    let encr_key = &protected[20..20 + encr_len];
    let check = &protected[20 + encr_len..];
    let pw = jks_passwd_utf16be(password_bytes);

    let mut xor_key = Vec::with_capacity(encr_len);
    let mut digest: Vec<u8> = salt.to_vec();
    while xor_key.len() < encr_len {
        let mut h = Sha1::new();
        h.update(&pw);
        h.update(&digest);
        digest = h.finalize().to_vec();
        xor_key.extend_from_slice(&digest);
    }
    let plain: Vec<u8> = encr_key
        .iter()
        .zip(xor_key.iter())
        .map(|(a, b)| a ^ b)
        .collect();

    // Integrity: SHA1(passwd || plain) must equal the trailing digest.
    let mut hc = Sha1::new();
    hc.update(&pw);
    hc.update(&plain);
    let computed = hc.finalize();
    if !constant_time_eq(&computed, check) {
        tracing::warn!(target: "keystore", "JKS key integrity check failed (wrong password?)");
        return None;
    }
    Some(plain)
}

/// DER length prefix for `n` content bytes (short form under 128, else the
/// minimal long form). Only lengths up to 2^24-1 occur here (a private key is
/// a few kilobytes at most).
fn der_len_bytes(n: usize) -> Vec<u8> {
    if n < 0x80 {
        vec![n as u8]
    } else if n <= 0xFF {
        vec![0x81, n as u8]
    } else if n <= 0xFFFF {
        vec![0x82, (n >> 8) as u8, n as u8]
    } else {
        vec![0x83, (n >> 16) as u8, (n >> 8) as u8, n as u8]
    }
}

/// `AlgorithmIdentifier { 1.3.6.1.4.1.42.2.17.1.1, NULL }` — Sun's frozen
/// JDK-1.2 `KeyProtector` algorithm id, the only one a JKS `PrivateKeyEntry`
/// ever carries. Written out verbatim; `der_extract_epki_octets` (the read
/// side) skips the whole SEQUENCE, and a real JDK's `AlgorithmId.parse`
/// accepts the explicit NULL parameters.
const JKS_KEY_PROTECTOR_ALG_ID: [u8; 16] = [
    0x30, 0x0E, 0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x2A, 0x02, 0x11, 0x01, 0x01, 0x05, 0x00,
];

/// Wrap a plaintext PKCS#8 private key in Sun's JKS `KeyProtector` envelope —
/// the exact inverse of [`jks_recover_key`].
///
/// STUB-REMOVAL (wave 2) / wave-1 follow-up: `write_jks` used to emit the
/// PLAINTEXT PKCS#8 bytes where the format (and every real JDK reading the
/// file) requires an `EncryptedPrivateKeyInfo`. Two consequences, both bad:
/// a keystore CratonVM wrote could not be read back by a real JDK at all, and
/// — far worse — `KeyStore.store()` silently wrote unprotected private keys to
/// disk for a caller who had just supplied a password precisely to prevent
/// that.
///
/// Envelope: `SEQUENCE { AlgorithmIdentifier, OCTET STRING }` where the octets
/// are `salt(20) || (plainPkcs8 XOR keystream) || SHA1(passwdUtf16be ||
/// plainPkcs8)` and the keystream is `Wi = SHA1(passwdUtf16be || W(i-1))` with
/// `W0 = salt`.
///
/// Returns `None` when the OS entropy source is unavailable, so the caller can
/// decide what to do rather than fall back to a predictable salt.
///
/// KNOWN LIMITATION (documented, not silently papered over): JKS permits a
/// per-entry key password distinct from the store password, but a loaded
/// `KeyStoreEntry` does not carry one — `load_jks` decrypts with whatever
/// password it was given and keeps only the plaintext. `write_jks` therefore
/// protects every key with the STORE password, which is correct for the
/// overwhelmingly common (and every fixture's) case where the two are equal,
/// and changes the key password to the store password otherwise.
fn jks_protect_key(plain_pkcs8: &[u8], password_bytes: &[u8]) -> Option<Vec<u8>> {
    use sha1::{Digest, Sha1};
    let mut salt = [0u8; 20];
    if !crate::securerandom::os_random_bytes(&mut salt) {
        return None;
    }
    let pw = jks_passwd_utf16be(password_bytes);

    let mut xor_key: Vec<u8> = Vec::with_capacity(plain_pkcs8.len() + 20);
    let mut digest: Vec<u8> = salt.to_vec();
    while xor_key.len() < plain_pkcs8.len() {
        let mut h = Sha1::new();
        h.update(&pw);
        h.update(&digest);
        digest = h.finalize().to_vec();
        xor_key.extend_from_slice(&digest);
    }
    let cipher: Vec<u8> = plain_pkcs8
        .iter()
        .zip(xor_key.iter())
        .map(|(a, b)| a ^ b)
        .collect();

    let mut hc = Sha1::new();
    hc.update(&pw);
    hc.update(plain_pkcs8);
    let check = hc.finalize();

    let mut protected = Vec::with_capacity(20 + cipher.len() + 20);
    protected.extend_from_slice(&salt);
    protected.extend_from_slice(&cipher);
    protected.extend_from_slice(&check);

    let mut octet = Vec::with_capacity(protected.len() + 5);
    octet.push(0x04);
    octet.extend_from_slice(&der_len_bytes(protected.len()));
    octet.extend_from_slice(&protected);

    let content_len = JKS_KEY_PROTECTOR_ALG_ID.len() + octet.len();
    let mut out = Vec::with_capacity(content_len + 5);
    out.push(0x30);
    out.extend_from_slice(&der_len_bytes(content_len));
    out.extend_from_slice(&JKS_KEY_PROTECTOR_ALG_ID);
    out.extend_from_slice(&octet);
    Some(out)
}

/// Whether a JKS key entry is still wrapped in Sun's KeyProtector envelope.
/// Is this key material still inside an encryption envelope?
///
/// Two envelopes reach `engineGetKey` on this VM, and neither is a private key:
///
///  * the JKS key protector, which [`is_jks_encrypted_private_key`] recognises
///    by its OID and which `load_jks` stores verbatim until a `getKey`
///    password opens it;
///  * a PKCS#12 `EncryptedPrivateKeyInfo`, which `load_pkcs12` keeps as a
///    placeholder `key_der` when the STORE password did not open the shrouded
///    bag (its `unwrap_or_else` arm says so) -- deliberately, so alias and
///    chain pairing still work for an entry whose key is separately protected.
///
/// The discrimination is exact rather than heuristic. A plaintext PKCS#8
/// `PrivateKeyInfo` opens with an INTEGER version where an
/// `EncryptedPrivateKeyInfo` opens with an `AlgorithmIdentifier` SEQUENCE, so
/// a real key cannot parse as an envelope.
pub fn is_encrypted_private_key(der: &[u8]) -> bool {
    if is_jks_encrypted_private_key(der) {
        return true;
    }
    yasna::parse_ber(der, p12::EncryptedPrivateKeyInfo::parse).is_ok()
}

/// Does this SPI's store type protect keys with a PKCS#12 envelope?
///
/// JKS and JCEKS both answer `false`: this VM writes a JCEKS store as JKS
/// records under `JCEKS_MAGIC` (see [`write_jks_with_magic`]) rather than with
/// SunJCE's PBE, so the envelope a JCEKS entry must carry is the JKS one.
/// Decided by receiver class for the reason [`spi_supports_secret_keys`] gives:
/// `NativeCallback` is a bare `fn` pointer and cannot capture its FQN.
fn spi_wants_pkcs12_envelope(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    class_name_of(ctx, this).starts_with("sun/security/pkcs12/")
}

/// Wrap `plain` under `password` in the envelope `pkcs12` selects.
///
/// KS-5: the envelope has to match the format the store will be WRITTEN in, so
/// `engineStore` can pass it through verbatim instead of re-encrypting with a
/// password the entry never chose.
fn protect_key_for_store(pkcs12: bool, plain: &[u8], password: &[u8]) -> Option<Vec<u8>> {
    if pkcs12 {
        let epki = pbes2_encrypt(plain, password)?;
        Some(yasna::construct_der(|w| epki.write(w)))
    } else {
        jks_protect_key(plain, password)
    }
}

/// Open EITHER envelope with `password`, or answer `None`.
///
/// KS-6: `load_pkcs12_ex` keeps a shrouded bag the STORE password did not open
/// as a placeholder `key_der`, and nothing downstream re-attempted it with the
/// ENTRY password `getKey` receives -- so a HotSpot-written PKCS#12 whose two
/// passwords differ handed the envelope back as a `PrivateKey` for every
/// password INCLUDING the correct one. JKS escaped that only because
/// `keystore_unlock_private_keys` already re-attempted its own scheme; this
/// makes that funnel scheme-blind, which is what its own doc already claimed.
///
/// The decrypt chain is deliberately the same one `load_pkcs12_ex` uses on the
/// way in, in the same order ([`open_pkcs12_envelope`], with the same
/// PKCS#8 acceptor since gc-common w33-c), so a bag that opens at load time
/// and a bag that opens at `getKey` time cannot disagree about what "opens"
/// means.
pub fn recover_key_any_scheme(envelope: &[u8], password: &[u8]) -> Option<Vec<u8>> {
    if let Some(plain) = jks_recover_key(envelope, password) {
        return Some(plain);
    }
    let epk = yasna::parse_ber(envelope, p12::EncryptedPrivateKeyInfo::parse).ok()?;
    let bmp = pkcs12_bmp_string(&String::from_utf8_lossy(password));
    // gc-common w33-c: only a PKCS#8 plaintext opens it. The JKS arm above is
    // authenticated (its SHA-1 check); these three are not, and a WRONG
    // password used to succeed about one time in 128 (padding that happened
    // to fit) -- `getKey` then handed back noise instead of throwing, and
    // `keystore_unlock_private_keys` wrote that noise over the entry for good.
    // See `open_pkcs12_envelope`.
    open_pkcs12_envelope(&epk, password, &bmp, pkcs8_private_key_info).or_else(|| {
        recover_jceks_pbe_key(&epk, password).and_then(|plain| pkcs8_private_key_info(&plain))
    })
}

/// KS-7: recover a real `com.sun.crypto.provider.JceKeyStore` private-key
/// entry. Its envelope is a PKCS#8 `EncryptedPrivateKeyInfo` like the other
/// two schemes above, but under a THIRD algorithm identifier — SunJCE's own
/// proprietary `PBEWithMD5AndTripleDES` (OID `1.3.6.1.4.1.42.2.19.1`,
/// `JAVASOFT_JCEKeyProtector`) — which is neither the JKS `KeyProtector`
/// envelope (this VM's own JCEKS writer's choice, since `write_jks_with_magic`
/// shares the JKS record layout under `JCEKS_MAGIC`) nor PKCS#12's PBES2/
/// legacy RC2/3DES bags the two arms above try. Reverse-engineered from
/// `com.sun.crypto.provider.{JceKeyStore,KeyProtector,PBES1Core}`, read via
/// `javap`/decompiler against the installed JDK's `src.zip` — not shipped as
/// source in a normal JDK distribution.
fn recover_jceks_pbe_key(epk: &p12::EncryptedPrivateKeyInfo, password: &[u8]) -> Option<Vec<u8>> {
    let p12::AlgorithmIdentifier::OtherAlg(alg) = &epk.encryption_algorithm else {
        return None;
    };
    // 1.3.6.1.4.1.42.2.19.1, JAVASOFT_JCEKeyProtector.
    match alg.algorithm_type.components().as_slice() {
        [1, 3, 6, 1, 4, 1, 42, 2, 19, 1] => {}
        _ => return None,
    }
    // PBEParameter ::= SEQUENCE { salt OCTET STRING, iterationCount INTEGER }
    let (salt, iter_count): (Vec<u8>, u32) = yasna::parse_ber(alg.params.as_deref()?, |r| {
        r.read_sequence(|r| {
            let salt = r.next().read_bytes()?;
            let iter_count = r.next().read_u32()?;
            Ok((salt, iter_count))
        })
    })
    .ok()?;
    // Same ceiling `KeyProtector.recover` itself enforces (`MAX_ITERATION_COUNT
    // = 5000000`) -- a forged count must not turn a `getKey` call into an
    // unbounded MD5 loop.
    if salt.len() != 8 || iter_count == 0 || iter_count > 5_000_000 {
        return None;
    }
    let salt: [u8; 8] = salt.try_into().ok()?;
    let (key, iv) = jceks_pbe_derive_key_iv(password, salt, iter_count);

    use aes::cipher::block_padding::Pkcs7;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecryptMut, KeyIvInit};
    let mut out = vec![0u8; epk.encrypted_data.len()];
    let written = cbc::Decryptor::<des::TdesEde3>::new(
        GenericArray::from_slice(&key),
        GenericArray::from_slice(&iv),
    )
    .decrypt_padded_b2b_mut::<Pkcs7>(&epk.encrypted_data, &mut out)
    .ok()?
    .len();
    out.truncate(written);
    Some(out)
}

/// Derive the 24-byte 3DES key and 8-byte IV `PBEWithMD5AndTripleDES` uses,
/// bit for bit as `com.sun.crypto.provider.PBES1Core.deriveCipherKey`'s
/// `DESede` branch (that class's own doc comment states the algorithm; this
/// is a direct transcription, not a guess):
///
/// 1. Split the 8-byte salt into two 4-byte halves; if the halves are equal,
///    invert the first one (swap `[0]<->[3]`, `[1]<->[2]`) so the two digest
///    chains below never start from the same seed.
/// 2. For each half: `digest = MD5(half || password)`, then repeat
///    `digest = MD5(digest || password)` for `iterationCount - 1` more
///    rounds -- `iterationCount` total MD5 operations per half.
/// 3. The two 16-byte digests concatenate to 32 bytes; the first 24
///    (`digest0 || digest1[0..8]`) form the 3DES key, the last 8
///    (`digest1[8..16]`) form the IV.
fn jceks_pbe_derive_key_iv(password: &[u8], salt: [u8; 8], iter_count: u32) -> ([u8; 24], [u8; 8]) {
    use md5::{Digest, Md5};
    let mut salt = salt;
    if salt[0..4] == salt[4..8] {
        salt.swap(0, 3);
        salt.swap(1, 2);
    }
    let mut result = [0u8; 32];
    for (i, half) in [&salt[0..4], &salt[4..8]].into_iter().enumerate() {
        let mut h = Md5::new();
        h.update(half);
        h.update(password);
        let mut digest = h.finalize_reset();
        for _ in 1..iter_count {
            h.update(digest);
            h.update(password);
            digest = h.finalize_reset();
        }
        result[i * 16..i * 16 + 16].copy_from_slice(&digest);
    }
    let mut key = [0u8; 24];
    key.copy_from_slice(&result[0..24]);
    let mut iv = [0u8; 8];
    iv.copy_from_slice(&result[24..32]);
    (key, iv)
}

pub fn is_jks_encrypted_private_key(der: &[u8]) -> bool {
    const JKS_KEY_PROTECTOR_OID: &[u8] = b"\x06\x0a\x2b\x06\x01\x04\x01\x2a\x02\x11\x01\x01";
    der.windows(JKS_KEY_PROTECTOR_OID.len())
        .any(|window| window == JKS_KEY_PROTECTOR_OID)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

struct JksReader<'a> {
    data: &'a [u8],
    cursor: usize,
}

impl<'a> JksReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, cursor: 0 }
    }
    fn pos(&self) -> usize {
        self.cursor
    }
    /// Bytes left in the buffer from the current cursor. Used to bound
    /// attacker-controlled element counts before reserving capacity, so a
    /// forged count can never drive a huge `Vec::with_capacity` (OOM DoS).
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor)
    }
    fn need(&self, n: usize) -> Result<(), KeyStoreError> {
        if self.cursor + n > self.data.len() {
            Err(KeyStoreError::Truncated(self.cursor))
        } else {
            Ok(())
        }
    }
    fn u32_be(&mut self) -> Result<u32, KeyStoreError> {
        self.need(4)?;
        let v = u32::from_be_bytes([
            self.data[self.cursor],
            self.data[self.cursor + 1],
            self.data[self.cursor + 2],
            self.data[self.cursor + 3],
        ]);
        self.cursor += 4;
        Ok(v)
    }
    fn u64_be(&mut self) -> Result<u64, KeyStoreError> {
        self.need(8)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.data[self.cursor..self.cursor + 8]);
        self.cursor += 8;
        Ok(u64::from_be_bytes(buf))
    }
    fn u16_be(&mut self) -> Result<u16, KeyStoreError> {
        self.need(2)?;
        let v = u16::from_be_bytes([self.data[self.cursor], self.data[self.cursor + 1]]);
        self.cursor += 2;
        Ok(v)
    }
    fn utf8_u16len(&mut self) -> Result<String, KeyStoreError> {
        let len = self.u16_be()? as usize;
        self.need(len)?;
        let s = String::from_utf8_lossy(&self.data[self.cursor..self.cursor + len]).into_owned();
        self.cursor += len;
        Ok(s)
    }
    fn bytes_u32len(&mut self) -> Result<Vec<u8>, KeyStoreError> {
        let len = self.u32_be()? as usize;
        self.need(len)?;
        let v = self.data[self.cursor..self.cursor + len].to_vec();
        self.cursor += len;
        Ok(v)
    }
}

/// The alias `sun.security.pkcs12.PKCS12KeyStore` gives a bag that carries no
/// `friendlyName`: `getUnfriendlyName()`, i.e. `++counter` rendered as a
/// decimal string, shared across every entry kind in one load.
///
/// See the call sites in `parse_pkcs12` for why the previous "hex of the
/// localKeyId" rule (what *keytool* prints, not what the reader assigns) made
/// every alias in netty's `mutual_auth_server.p12` unfindable.
fn unfriendly_alias(counter: &mut u32) -> String {
    *counter += 1;
    counter.to_string()
}

fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{:02x}", x));
    }
    s
}

// ---------------------------------------------------------------------------
// Native registration
// ---------------------------------------------------------------------------

/// Every class the provider table advertises as a `KeyStore` implementation,
/// so the guard below and the registrations above cannot drift apart silently.
#[cfg(test)]
const ADVERTISED_KEYSTORE_CLASSES: &[&str] = &[
    "sun/security/provider/JavaKeyStore$DualFormatJKS",
    "sun/security/provider/JavaKeyStore$CaseExactJKS",
    "sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12",
    "sun/security/provider/DomainKeyStore$DKS",
    "sun/security/pkcs12/PKCS12KeyStore",
    // SunJCE's `KeyStore.JCEKS`. The row in `provider_chain`, this one and the
    // `register_engine_surface` call are joined only by a string, which is what
    // `advertised_keystore_registration_tests` below exists to check -- see its
    // doc comment for the 754-to-563 regression that gap once caused, and note
    // that it caught this entry on the day it was added, with the engine
    // surface missing.
    JCEKS_FQN,
];

#[cfg(test)]
mod advertised_keystore_registration_tests;

/// FQNs we register on. PKCS12 + JKS share the same `engine*` surface; we
/// register on each FQN explicitly because dispatch is keyed by class name
/// (no Java-inheritance walk on the native side — `java/security/KeyStore`
/// itself reaches us via the existing `phases_early.rs` route which now
/// delegates to this module's helpers).
const PKCS12_FQN: &str = "sun/security/pkcs12/PKCS12KeyStore";
const JKS_FQN: &str = "sun/security/provider/JavaKeyStore";
const JKS_INNER_JKS_FQN: &str = "sun/security/provider/JavaKeyStore$JKS";
const JKS_INNER_DUAL_FQN: &str = "sun/security/provider/JavaKeyStore$DualFormatJKS";
/// SunJCE's `KeyStore.JCEKS`. Its on-disk format is JKS's under a different
/// magic (`JCEKS_MAGIC`), so it shares the whole engine surface; what it does
/// NOT share is the magic `engine_store` writes, which is keyed on this name.
const JCEKS_FQN: &str = "com/sun/crypto/provider/JceKeyStore";
/// `KeyStore.getInstance("PKCS12")` — the DEFAULT, and the one netty, Tomcat
/// and every `SslContextBuilder` reach for.
///
/// Registered late, and the omission cost 191 tests. When the SUN provider's
/// `KeyStore.PKCS12` row was corrected to name the real JDK class
/// (`…$DualFormatPKCS12`, a `KeyStoreDelegator` that sniffs the stream), the
/// row started naming a class this file does not register — and dispatch here
/// is keyed by CLASS NAME with no inheritance walk, as the comment above says.
/// So the engine surface silently went missing: every `engineLoad` on the
/// platform-default PKCS#12 store did nothing, netty's key material came back
/// empty, and `JdkSslEngineTest` went 754 ok / 1 failed to 563 / 192, with
/// `IOException: setNeedClientAuth(true) requires javax.net.ssl.trustStore`
/// among the wreckage — an engine falling back to `default_engine_server_config`
/// because its `SSLContext` never got an identity.
///
/// The JKS half of that same change WAS registered (`JKS_INNER_DUAL_FQN`
/// above). Only its twin was missed, which is why
/// `every_advertised_keystore_class_has_an_engine_surface` now exists.
const PKCS12_INNER_DUAL_FQN: &str = "sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12";
/// `KeyStore.getInstance("CaseExactJKS")` — advertised long before the row
/// correction and never registered either, so this one is not a regression,
/// just the same hole one door along.
const JKS_INNER_CASE_EXACT_FQN: &str = "sun/security/provider/JavaKeyStore$CaseExactJKS";
/// `KeyStore.getInstance("DKS")`. A domain keystore is a different FORMAT
/// (a policy file naming other stores), so serving it through this engine
/// surface is not right in the long run — but an unregistered class answers
/// nothing at all, and answering a `KeyStoreException` from a real
/// `engineLoad` is strictly closer to the JDK than a method with no body.
const DKS_FQN: &str = "sun/security/provider/DomainKeyStore$DKS";

const SUN_KEYSTORE_FQN: &str = "java/security/KeyStore";

/// The store-id slot of the SYNTHETIC `java.security.KeyStore` layout
/// (`tls.rs`, section 7: five raw slots, slot 4 the id). It is a slot only on
/// that layout: see [`store_id_slot_is_synthetic`] for why it must never be
/// read or written on a real class.
const FIELD_STORE_ID: usize = 4;

/// Is slot [`FIELD_STORE_ID`] of `this` the synthetic store-id slot, rather
/// than a real field of `this`'s class?
///
/// gc-common w34-c (`common-w33c-keystore-store-id-slot-overlays-a-real-spi-field-FIXED-20260923`):
/// the slot tier of [`get_store_id`] / [`set_store_id`] used to apply to EVERY
/// receiver with more than four slots. The receivers are mostly real JDK
/// classes -- `KeyStoreDelegator` (`KeyStore.getInstance("PKCS12")` and
/// `("JKS")`), whose slot 4 is its `type` String, and `PKCS12KeyStore`, whose
/// slot 4 is `certProtectionAlgorithm` -- and `set_field` coerces an `Int`
/// written into a reference slot to null. So every `engineLoad` nulled a real
/// field, and the read back answered nothing (the id was found by the identity
/// tier). Harmless only while no real bytecode read the field.
///
/// The slot is the synthetic one exactly when the receiver's class DECLARES
/// no field there: the synthetic layout is allocated with more slots than its
/// class declares (`try_alloc_concurrent_synthetic`'s `max(requested, real)`),
/// a real class's instance never is. A context that reports no layout (the
/// test mock answers 0 declared fields) is judged by the slot's content
/// instead: a live reference there is certainly not a store id.
fn store_id_slot_is_synthetic(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let slots = ctx.object_num_fields(this);
    if slots <= FIELD_STORE_ID {
        return false;
    }
    let declared = ctx.class_num_total_fields(ctx.class_id_of_object(this));
    if declared > FIELD_STORE_ID {
        // A real field: not even read (the overlay checker would flag it).
        return false;
    }
    store_id_slot_verdict(slots, declared, ctx.get_field(this, FIELD_STORE_ID))
}

/// [`store_id_slot_is_synthetic`]'s decision, over plain values.
fn store_id_slot_verdict(slots: usize, declared: usize, current: Value) -> bool {
    slots > FIELD_STORE_ID
        && declared <= FIELD_STORE_ID
        && !matches!(current, Value::Object(Some(_)))
}



/// Whether the JCEKS / DKS engine surfaces are left to the JDK's bytecode in
/// this registry.
///
/// JIT round 16 wave 17 (lane shadow): both modes target HotSpot. The natives
/// keep entries in this module's side table and serve DKS with the JKS engine,
/// so under `--compatible` they answered `R16ShadowRetire`'s J12/J16/J26/J28/J29
/// and D04-D20 rows differently from HotSpot (a reloaded JCEKS lost its secret
/// keys; DKS aliases had no domain prefix; a `DomainLoadStoreParameter` loaded
/// nothing). `--real-jdk` keeps registering them so the retirement table
/// re-tags and drops them there (the kind map is unchanged); the synthetic JDK
/// has no bytecode for either class and keeps them.
fn keystore_spi_left_to_bytecode(r: &NativeMethodRegistry) -> bool {
    r.real_jdk() && !r.compatibility_mode().is_jdk_only()
}

/// The engine surfaces `register_keystore_real` always registers: the
/// `--compatible` keystore carrier, whose side table the rustls TLS carrier
/// reads (`compat_carriers` classifies them `Intrinsic` there).
pub const CARRIER_ENGINE_CLASSES: &[&str] = &[
    PKCS12_FQN,
    JKS_FQN,
    JKS_INNER_JKS_FQN,
    JKS_INNER_DUAL_FQN,
    PKCS12_INNER_DUAL_FQN,
    JKS_INNER_CASE_EXACT_FQN,
];

/// Public entry point — wave coordinator wires this from `lib.rs`.
pub fn register_keystore_real(r: &mut NativeMethodRegistry) {
            for fqn in CARRIER_ENGINE_CLASSES {
        register_engine_surface(r, *fqn);
    }
    if !keystore_spi_left_to_bytecode(r) {
        register_engine_surface(r, DKS_FQN);
        register_engine_surface(r, JCEKS_FQN);
    }

    // The `java.security.KeyStore` shim's `load`/`getKey`/`getCertificate`
    // engine surface is registered by `phases_early.rs` — we don't override
    // its registrations (forbidden surface). Instead we re-export the
    // helpers it can call into through `crate::keystore::*` once the
    // wave coordinator wires this module.

    // `engine_aliases` returns a synthetic `java/util/IteratorEnumeration`
    // (array at slot 0, position at slot 1). Its `hasMoreElements`/`nextElement`
    // were only registered in the synthetic-JDK path
    // (`phases_early::register_phase53_security`, via `register_synthetic_overrides`),
    // which real-JDK mode never calls — so in real-JDK mode the TLS
    // `KeyManagerFactory`/`TrustManagerFactory` init that walks `ks.aliases()`
    // hit `NoSuchMethodError IteratorEnumeration.hasMoreElements()`. Register
    // them here (real-JDK path), co-located with the producer.
    r.register(
        "java/util/IteratorEnumeration",
        "hasMoreElements",
        "()Z",
        |ctx, args| {
            let this = this_arg(args)?;
            let pos = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let len = match ctx.get_field(this, 0) {
                Value::Object(Some(arr)) => ctx.array_length(arr),
                _ => 0,
            };
            Ok(Some(Value::Int(if pos < len { 1 } else { 0 })))        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/IteratorEnumeration",
        "nextElement",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this = this_arg(args)?;
            let pos = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let elem = match ctx.get_field(this, 0) {
                Value::Object(Some(arr)) => {
                    if pos < ctx.array_length(arr) {
                        ctx.set_field(this, 1, Value::Int((pos + 1) as i32));
                        ctx.get_array_element(arr, pos)
                    } else {
                        Value::Object(None)
                    }
                }
                _ => Value::Object(None),
            };
            Ok(Some(elem))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    let _ = SUN_KEYSTORE_FQN;
    }

fn register_engine_surface(r: &mut NativeMethodRegistry, fqn: &'static str) {
            // engineLoad(InputStream, char[])
    r.register(fqn, "engineLoad", "(Ljava/io/InputStream;[C)V", engine_load, cratonvm_native_api::NativeKind::Bridge);

    // engineGetKey(String, char[]) -> Key
    r.register(
        fqn,
        "engineGetKey",
        "(Ljava/lang/String;[C)Ljava/security/Key;",
        engine_get_key, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineGetCertificate(String) -> Certificate
    r.register(
        fqn,
        "engineGetCertificate",
        "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
        engine_get_certificate, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineGetCertificateChain(String) -> Certificate[]
    r.register(
        fqn,
        "engineGetCertificateChain",
        "(Ljava/lang/String;)[Ljava/security/cert/Certificate;",
        engine_get_certificate_chain, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineAliases() -> Enumeration<String>
    r.register(
        fqn,
        "engineAliases",
        "()Ljava/util/Enumeration;",
        engine_aliases, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineSize() -> int
    r.register(fqn, "engineSize", "()I", engine_size, cratonvm_native_api::NativeKind::Bridge);
    // The seventeenth `engine*`. Missing, it did not degrade -- it reached
    // real `KeyStoreDelegator` bytecode and raised NPE on a field this module
    // never populates. See `engine_get_certificate_alias`.
    r.register(
        fqn,
        "engineGetCertificateAlias",
        "(Ljava/security/cert/Certificate;)Ljava/lang/String;",
        engine_get_certificate_alias, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineContainsAlias(String) -> boolean
    r.register(
        fqn,
        "engineContainsAlias",
        "(Ljava/lang/String;)Z",
        engine_contains_alias, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineIsKeyEntry(String) -> boolean
    r.register(
        fqn,
        "engineIsKeyEntry",
        "(Ljava/lang/String;)Z",
        engine_is_key_entry, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineIsCertificateEntry(String) -> boolean
    r.register(
        fqn,
        "engineIsCertificateEntry",
        "(Ljava/lang/String;)Z",
        engine_is_certificate_entry, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineGetCreationDate(String) -> Date
    r.register(
        fqn,
        "engineGetCreationDate",
        "(Ljava/lang/String;)Ljava/util/Date;",
        engine_get_creation_date, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineSetCertificateEntry(String, Certificate) — in-memory mutation,
    // writes the same side-table the read natives consult (the real bytecode
    // updated a separate `entries` field invisible to engineAliases).
    r.register(
        fqn,
        "engineSetCertificateEntry",
        "(Ljava/lang/String;Ljava/security/cert/Certificate;)V",
        engine_set_certificate_entry, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineSetKeyEntry(String, Key, char[], Certificate[]) — companion to
    // engineSetCertificateEntry above for the PrivateKey case. See
    // keystore_set_key_entry's doc comment: without this, KeyManagerFactory
    // found no staged identity for an in-memory-only keystore built via
    // getInstance()+load(null,null)+setKeyEntry(...) (Netty's
    // JdkSslServerContext.buildKeyStore ephemeral self-signed-cert pattern),
    // and SSLContext.init produced a server-side SSLContext with no
    // certificate to present — the TLS handshake then failed immediately
    // ("unexpected EOF" on the client side). See
    // docs/feature-designs/native-dispatch-architecture.md.
    r.register(
        fqn,
        "engineSetKeyEntry",
        "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V",
        engine_set_key_entry, cratonvm_native_api::NativeKind::Bridge,
    );
    // engineDeleteEntry(String) — companion in-memory removal.
    r.register(
        fqn,
        "engineDeleteEntry",
        "(Ljava/lang/String;)V",
        engine_delete_entry, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineStore(OutputStream, char[]) — serialise the side-table (as JKS,
    // which CV's load path detects by magic) so a store→load round-trip
    // preserves entries.
    r.register(
        fqn,
        "engineStore",
        "(Ljava/io/OutputStream;[C)V",
        engine_store, cratonvm_native_api::NativeKind::Bridge,
    );

    // engineSetEntry / engineGetEntry — the `KeyStore.Entry`-shaped half of the
    // API. See `engine_set_entry`'s doc comment for why leaving these to real
    // bytecode was a silent data loss rather than merely a gap.
    r.register(
        fqn,
        "engineSetEntry",
        "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V",
        engine_set_entry, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fqn,
        "engineGetEntry",
        "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;",
        engine_get_entry, cratonvm_native_api::NativeKind::Bridge,
    );

    // The last three `engine*` a `KeyStoreDelegator` receiver can reach
    // (gc-common w34-c, `common-w33c-keystore-store-id-slot-overlays-a-real-spi-field-FIXED-20260923`
    // section 2). Unregistered, each ran the real delegator bytecode, which
    // dereferences `this.keystore` -- never populated, because `engineLoad` is
    // native -- and threw NullPointerException from
    // `KeyStore.getAttributes` / `entryInstanceOf` / `setKeyEntry(String,
    // byte[], Certificate[])` on every PKCS12 and JKS keystore. Real bridges
    // over the same side table as the other seventeen, under the `Bridge`
    // category set at the top of this function.
    r.register(
        fqn,
        "engineGetAttributes",
        "(Ljava/lang/String;)Ljava/util/Set;",
        engine_get_attributes, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fqn,
        "engineEntryInstanceOf",
        "(Ljava/lang/String;Ljava/lang/Class;)Z",
        engine_entry_instance_of, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fqn,
        "engineSetKeyEntry",
        "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V",
        engine_set_protected_key_entry, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// `engine*` callback implementations
// ---------------------------------------------------------------------------

/// The alias, as the JDK's own keystore SPIs key it: **NPE on null, lowercased
/// otherwise**.
///
/// Both halves are measured, in both modes
/// (`probes/KeyStoreFamilySweep.java`, HotSpot 25.0.3+9):
///
/// ```text
/// setCertificateEntry("MixedCaseAlias", c); aliases()
///   HotSpot  [mixedcasealias]      CratonVM  [MixedCaseAlias]
/// containsAlias("mixedcasealias")
///   HotSpot  true                  CratonVM  false
/// containsAlias(null)
///   HotSpot  NullPointerException  CratonVM  no-throw
/// ```
///
/// `PKCS12KeyStore` and `JavaKeyStore` both do `alias.toLowerCase(Locale
/// .ENGLISH)` before touching their map, which is where both behaviours come
/// from at once: the null dereference and the case folding. This VM read the
/// alias with `.unwrap_or_default()`, so a null alias silently became `""` and
/// a mixed-case one stayed mixed.
///
/// Why the case matters beyond a probe row: aliases are how a keystore file
/// written by `keytool` is addressed. A store written elsewhere and read here
/// (or the reverse) disagreed about every alias that was not already lower
/// case, and `containsAlias` answered false for a name the store really held.
///
/// `to_lowercase` rather than `to_ascii_lowercase`: Rust's is locale-INDEPENDENT
/// Unicode lowercasing, which is what `Locale.ENGLISH` selects; the ASCII form
/// would diverge on a non-ASCII alias, and the locale-sensitive form is the
/// Turkish-dotless-I trap the JDK pins `Locale.ENGLISH` to avoid.
fn alias_arg(ctx: &mut dyn NativeContext, args: &[Value]) -> Result<String, MethodCallFailed> {
    match args.get(1) {
        Some(v @ Value::Object(Some(_))) => match read_string_arg(ctx, v) {
            Some(a) => Ok(a.to_lowercase()),
            // A non-null object that is not a readable String: keep the old
            // lenient behaviour rather than inventing a second failure mode.
            None => Ok(String::new()),
        },
        _ => Err(RuntimeError::NullPointerException {
            message: Some("KeyStore alias is null".into()),
        }
        .into()),
    }
}

fn this_arg(args: &[Value]) -> Result<ObjectRef, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(r))) => Ok(*r),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("KeyStore engine call on null receiver".into()),
        }
        .into()),
    }
}

pub fn read_password(ctx: &mut dyn NativeContext, v: &Value) -> Vec<u8> {
    if let Value::Object(Some(arr)) = v {
        let len = ctx.array_length(*arr);
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            // char[] in our model is Value::Int holding the 16-bit codepoint.
            // We project to 0..=0xFF (Latin-1) for the JKS path; PKCS#12
            // path uses the str roundtrip below. Anything > 0xFF gets its
            // low byte; passwords with non-Latin-1 chars are exotic.
            if let Value::Int(c) = ctx.get_array_element(*arr, i) {
                out.push((c & 0xFF) as u8);
            }
        }
        out
    } else {
        Vec::new()
    }
}

/// Pull every byte the `InputStream` will give us until EOF.
///
/// We try the cheap path first: if the stream is a `ByteArrayInputStream`
/// (the common case in real-JDK keystore loading — the JKS code wraps the
/// input bytes in BAIS internally, and most callers also pass a BAIS), we
/// can skip the bytecode-level read loop and pull bytes straight out of
/// the backing array. Field layout: `buf=0, pos=1, mark=2, count=3`.
///
/// If that doesn't apply, fall back to invoking
/// `InputStream.read(byte[], int, int)` in a loop. This is slower because
/// each invocation is a full bytecode trip, but it's the only correct path
/// for arbitrary streams (FileInputStream, network streams, gzip, ...).
fn read_stream_to_end(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Vec<u8> {
    // Cheap path: ByteArrayInputStream
    if matches!(ctx.heap_kind_of(stream), cratonvm_types::ObjectKind::Object) {
        let cls_id = ctx.class_id_of_object(stream);
        if let Some(name) = ctx.class_name_of_id(cls_id) {
            if name == "java/io/ByteArrayInputStream" {
                let buf = ctx.get_field(stream, 0);
                let pos = match ctx.get_field(stream, 1) {
                    Value::Int(v) => v as usize,
                    _ => 0,
                };
                let count = match ctx.get_field(stream, 3) {
                    Value::Int(v) => v as usize,
                    _ => 0,
                };
                if let Value::Object(Some(arr)) = buf {
                    let total = ctx.array_length(arr).min(count);
                    let start = pos.min(total);
                    let mut out = Vec::with_capacity(total - start);
                    for i in start..total {
                        if let Value::Int(b) = ctx.get_array_element(arr, i) {
                            out.push(b as u8);
                        }
                    }
                    return out;
                }
            }
        }
    }

    // General path: invoke `int read(byte[], int, int)` in a loop.
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    let chunk_size = 4096usize;
    let chunk = ctx.new_array(ArrayElementType::Byte, chunk_size);
    let chunk_pin = ctx.pin_native_root(chunk);
    let stream_pin = ctx.pin_native_root(stream);
    loop {
        let chunk = ctx.read_native_pin(chunk_pin, chunk);
        let stream = ctx.read_native_pin(stream_pin, stream);
        let res = ctx.invoke(
            "java/io/InputStream",
            "read",
            "([BII)I",
            &[
                Value::Object(Some(stream)),
                Value::Object(Some(chunk)),
                Value::Int(0),
                Value::Int(chunk_size as i32),
            ],
        );
        let n = match res {
            Ok(Some(Value::Int(n))) => n,
            // Any error or surprising return: stop trying. We may have
            // partial data; downstream parsers will detect a Truncated error
            // and surface it.
            _ => break,
        };
        if n <= 0 {
            break;
        }
        // gc-common w20-e: `read` ran Java; the bytes it wrote are in the
        // chunk's CURRENT copy. They used to be copied out of the address
        // read before the call.
        let chunk = ctx.read_native_pin(chunk_pin, chunk);
        for i in 0..(n as usize) {
            if let Value::Int(b) = ctx.get_array_element(chunk, i) {
                out.push(b as u8);
            }
        }
    }
    // The two pins were never released: every chunked load left them on the
    // native pin stack until the outermost native returned.
    ctx.unpin_native_roots(chunk_pin);
    out
}

pub fn engine_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    // Pinned across the stream read below (gc-common w11-a): it runs
    // `InputStream.read` and allocates its chunk array, so a moving
    // collection can relocate the receiver, and `set_store_id` then wrote the
    // store id through, and took the identity hash of, the stale address.
    let this_pin = ctx.pin_native_root(this);

    // null InputStream is "create empty". Real-JDK does the same.
    let stream_opt = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    // A Java `null` char[] (as opposed to a present-but-empty one) means "no
    // password supplied" and must disable PKCS#12 MAC/integrity checking —
    // see `load_pkcs12_ex`. Distinguish the two here, since `read_password`
    // collapses both to an empty Vec (correct for the entry-decryption call
    // sites that use it, which don't care about the distinction).
    let password_arg = args.get(2);
    let password_present = !matches!(password_arg, None | Some(Value::Object(None)));
    let password = match password_arg {
        Some(v) => read_password(ctx, v),
        None => Vec::new(),
    };

    let bytes = match stream_opt {
        Some(s) => read_stream_to_end(ctx, s),
        None => Vec::new(),
    };
    // Nothing below calls Java or allocates on the Java heap.
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);

    let store = if bytes.is_empty() {
        LoadedKeyStore::default()
    } else {
        match load_keystore_ex(&bytes, &password, password_present) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "keystore", "engineLoad: parse failed: {e}");
                // Match real-JDK: throw IOException. For simplicity here we
                // surface a runtime error — callers (real-JDK glue) wrap
                // this as IOException at the bytecode boundary.
                return Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(RuntimeError::IOException {
                        message: e.to_string(),
                    }),
                ));
            }
        }
    };

    // Bridge the parsed keystore into the rustls-backed TLS engine: install the
    // first key entry as the server identity.
    //
    // FIX (es-restclient-https): trust anchors ARE now also mirrored here (see
    // below), reversing the previous "TrustManagerFactory/SSLContext scope
    // them instead" assumption. That assumption relied on
    // `TrustManagerFactory.init(KeyStore)`'s native firing reliably — mirroring
    // the same (correct, see below) assumption made for `KeyManagerFactory
    // .init` — but empirically, `TrustManagerFactory.getInstance("PKIX")`
    // (the JDK default algorithm) resolves to
    // `sun.security.ssl.TrustManagerFactoryImpl$PKIXFactory`, whose
    // `engineInit(KeyStore)` is REAL, non-native bytecode (inherited from the
    // abstract `TrustManagerFactoryImpl.engineInit`) that CratonVM does not
    // intercept — only the SunX509 `$SimpleFactory` variant has a registered
    // native (`x509_manager.rs::tmf_engine_init`). So for the (common) PKIX
    // default algorithm, no native ever runs to call
    // `set_pending_tm_trust_roots`, and a caller-supplied truststore's anchors
    // were silently dropped: `SSLContext.init` always fell back to the
    // platform root store only, so a self-signed/test-CA certificate (e.g.
    // the `HttpsServer`+`RestClient` pattern in `RestClientBuilderIntegTests`)
    // always failed handshake with `UnknownIssuer`. `engineLoad` is the one
    // reliable native call point regardless of which `TrustManagerFactory`
    // algorithm/impl class ends up being used, so stage the anchors here too.
    let mut first_key_identity: Option<(Vec<u8>, Vec<Vec<u8>>)> = None;
    let mut trust_anchor_ders: Vec<Vec<u8>> = Vec::new();
    for entry in store.entries.values() {
        match &entry.kind {
            EntryKind::PrivateKey { key_der, chain, .. } => {
                if !is_jks_encrypted_private_key(key_der) {
                    crate::t27_tls::install_identity_from_der(ctx.vm_identity(), key_der, chain);
                    if first_key_identity.is_none() {
                        first_key_identity = Some((key_der.clone(), chain.clone()));
                    }
                }
                // The chain's root (last cert) is a trust anchor too — a
                // keystore holding a self-signed identity (the common test
                // pattern: `keytool -genkeypair`) IS its own trust anchor.
                if let Some(root) = chain.last() {
                    trust_anchor_ders.push(root.clone());
                }
            }
            EntryKind::TrustedCert { cert_der } => {
                trust_anchor_ders.push(cert_der.clone());
            }
            EntryKind::SecretKey { .. } => {}
        }
    }

    // A reload of a store this VM already registered for this object REPLACES
    // it (gc-common w11-a, see `keystore_replace_in_vm`); its identity PEM
    // snapshot goes too, so a reloaded store without a key entry does not keep
    // serving the old one.
    let vm = ctx.vm_identity();
    let previous = get_store_id(ctx, this);
    let id = match keystore_replace_in_vm(vm, previous, store) {
        Ok(()) => {
            store_identity_pem_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&previous);
            previous
        }
        Err(store) => keystore_register_in_vm(vm, store),
    };
    set_store_id(ctx, this, id);
    // Record this keystore's identity PEM keyed by store_id, AND stage it on the
    // current thread for the next `SSLContext.init`. In real-JDK mode the
    // `KeyManagerFactory.init` natives are synthetic-gated (the real bytecode
    // runs), so `engineLoad` — which IS a real-mode native — is the reliable
    // capture point: a keystore is loaded immediately before its KMF/SSLContext
    // is built on the same thread (e.g. TesterSupport.getUserKeyManagers →
    // SSLContext.init). `SSLContext.init` consumes (clears) the thread-local, so
    // it only sticks to the very next context built after this load.
    if let Some((key_der, chain)) = first_key_identity {
        let (cert_pem, key_pem) = crate::t27_tls::der_identity_to_pem(&key_der, &chain);
        store_identity_pem_map()
            .lock()
            .unwrap()
            .insert(id, (cert_pem.clone(), key_pem.clone()));
        crate::t27_tls::set_pending_km_identity(cert_pem, key_pem);
    }
    // FIX (es-restclient-https): stage this keystore's trust anchors for the
    // next `SSLContext.init`, same lifetime/consumption rules as the KM
    // identity above (one-shot thread-local, cleared by `SSLContext.init`).
    if !trust_anchor_ders.is_empty() {
        crate::t27_tls::set_pending_tm_trust_roots(trust_anchor_ders);
    }
    Ok(Some(Value::Object(None)))
}

pub fn engine_get_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let password = args
        .get(2)
        .map(|v| read_password(ctx, v))
        .unwrap_or_default();
    // A JKS store may be loaded without its per-entry key password (Tomcat's
    // `Ssl` configuration supplies that password later through getKey). Unlock
    // the entry at the API boundary that actually receives it, so consumers
    // never receive an EncryptedPrivateKeyInfo masquerading as PKCS#8.
    keystore_unlock_private_keys(ctx.vm_identity(), id, &password);
    let alias = alias_arg(ctx, args)?;
    // The one entry is copied out, not the store (gc-common w33-c).
    let Some(entry) = with_store(id, |store| store.entries.get(&alias).cloned()).flatten() else {
        return Ok(Some(Value::Object(None)));
    };

    if let EntryKind::PrivateKey {
        key_der,
        protected: entry_envelope,
        ..
    } = &entry.kind
    {
        // KS-5, the IN-MEMORY half. An entry set through the API keeps its
        // plaintext in `key_der`, so the envelope check below -- which asks
        // whether `key_der` is still ciphertext -- cannot see a wrong password
        // here and never could: there is no ciphertext to look at. Ask the
        // envelope the ENTRY password built instead, which is the only thing
        // in this entry that knows what that password was.
        //
        // HotSpot refuses the same call for the same reason: SunPKCS12 keeps
        // the key encrypted in memory and decrypts inside `engineGetKey`.
        if let Some(env) = entry_envelope.as_deref() {
            if recover_key_any_scheme(env, &password).is_none() {
                let msg = if is_jks_encrypted_private_key(env) {
                    "Cannot recover key"
                } else {
                    "Get Key failed: Given final block not properly padded. Such issues can arise if a bad key is used during decryption."
                };
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/UnrecoverableKeyException",
                    msg,
                ));
            }
        }
        // KS-1: the password did not open this entry, so there is no key to
        // return. Before this check the still-encrypted envelope was wrapped in
        // the `PrivateKey` mirror below and handed to the application, which
        // cannot tell: the mirror answers `getAlgorithm()` and `getFormat()`
        // from fields, so ciphertext reads as `RSA/PKCS#8` and only
        // `getEncoded()` -> `KeyFactory` shows it is not a key.
        //
        // MEASURED 2026-09-10 against HotSpot 25.0.3+9
        // (`KSInteropWriteRead.java`, and the matrix in
        // `jdk-only/keystore-getkey-accepts-any-
        //  password-and-the-key-does-not-round-trip-20260910.md`): on a
        // HotSpot-written JKS this VM already DISCRIMINATES correctly: the
        // entry password yields a usable key and the other two yield the
        // envelope, so the whole of the defect on that path was returning it
        // instead of throwing. `keystore_unlock_private_keys` above is the verification;
        // this is the verdict.
        //
        // The message is chosen by the ENVELOPE, not the store type, because
        // the envelope is what this VM actually holds: HotSpot's JKS
        // `KeyProtector` says "Cannot recover key", and SunPKCS12 reports the
        // JCE padding failure behind a "Get Key failed: " prefix.
        if is_encrypted_private_key(key_der) {
            let msg = if is_jks_encrypted_private_key(key_der) {
                "Cannot recover key"
            } else {
                "Get Key failed: Given final block not properly padded. Such issues can arise if a bad key is used during decryption."
            };
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/UnrecoverableKeyException",
                msg,
            ));
        }
        // Allocate the synthetic PrivateKey mirror. Field layout matches the
        // existing convention (algo_idx=0, key_size_bits=1, key_len_bytes=2,
        // key_id=3) so the TLS path keeps working. We additionally stash the
        // store_id + alias hash in a registry so the TLS layer can pull the
        // DER through `keystore_get_private_key()` rather than needing to
        // round-trip through this object.
        let pk = try_alloc_concurrent_synthetic(ctx, "java/security/PrivateKey", 4)?;
        let algo_idx = detect_algo_idx(key_der);
        ctx.set_field(pk, 0, Value::Int(algo_idx));
        ctx.set_field(pk, 1, Value::Int((key_der.len() as i32).saturating_mul(8)));
        ctx.set_field(pk, 2, Value::Int(key_der.len() as i32));
        // store_id encoded into the high 32 bits, alias-hash in the low 32.
        let alias_hash = fnv1a_32(alias.as_bytes());
        let composite = ((id as i64 & 0xFFFF_FFFF) << 32) | (alias_hash as i64 & 0xFFFF_FFFF);
        ctx.set_field(pk, 3, Value::Long(composite));
        // gc-common w14-f: the proxy resolves its DER through the store
        // (`private_key_der_from_proxy`) for as long as it lives, whether or
        // not the `KeyStore` does. `pk` was just allocated: current.
        note_store_carrier(&*ctx, pk, id);
        // FIX (sslWithPemCertificates-decrypterror-20260720): `jca::signature`'s
        // `extract_key_id_from_key` reads THIS SAME field slot 3 as a
        // `crypto_impl` RSA key_id when the key's `identityHashCode` isn't
        // found in `rsa_realkey_map` first — but slot 3 here is the
        // (store_id, alias_hash) composite above, a completely different
        // namespace. `Signature.sign()` on a `KeyStore.getKey()`-sourced
        // PrivateKey therefore signed with whatever unrelated key happened to
        // occupy that same numeric id in `crypto_impl`'s RSA_KEY_STORE (or
        // silently produced a bad signature), causing rustls's TLS 1.3
        // CertificateVerify check to fail on the peer with `BadSignature` /
        // `DecryptError` during mTLS — reproduced in isolation (no TLS
        // involved) by round-tripping `Signature.sign()`/`verify()` on a
        // `KeyStore.getKey()`-sourced PKCS12 RSA key: verify failed on
        // CratonVM, succeeded on HotSpot, with byte-identical key material.
        // Register the real key material under this object's identity hash —
        // exactly like `register_rsa_priv_sign_material` does for
        // `KeyFactory.generatePrivate` imports — so `extract_key_id_from_key`
        // finds the correct key_id before ever falling through to slot 3.
        if algo_idx == 6 {
            if let Some(kp) = crypto_impl::parse_rsa_private_key_pkcs8(key_der) {
                let key_id = crypto_impl::rsa_key_next_id();
                crypto_impl::rsa_key_store(key_id, kp);
                // gc-common w18-e (`common-w17b-crypto-impl-key-stores-are-never-freed`):
                // the hold makes the handle tracked, so `pk`'s row below owns
                // it. Every `getKey` stores a fresh pair; it now goes with the
                // proxy that carries it instead of staying for the process.
                let _hold = crypto_impl::hold_key_handle(crypto_impl::KeyStoreKind::Rsa, key_id);
                // Keyed by `pk`'s lock key -- see `crypto_impl::RSA_REALKEY_MAP`'s
                // doc comment. `pk` is current: nothing allocated since
                // `note_store_carrier`.
                crypto_impl::rsa_realkey_map_set(&*ctx, pk, key_id);
            }
        }
        Ok(Some(Value::Object(Some(pk))))
    } else if let EntryKind::SecretKey {
        key_bytes,
        algorithm,
    } = &entry.kind
    {
        // Return the concrete mirror rather than the `SecretKey` interface:
        // SmallRye asks `Key.getEncoded()`, whose real interface method has no
        // code body. `SecretKeySpec` has registered accessors and preserves
        // the raw key bytes in field 0.
        //
        // gc-common w20-e: the key used to be allocated first and filled
        // through that address after the byte array's and the algorithm
        // String's allocations. Payload first, rooted; the key last.
        let mut scope = NativeHandleScope::new(ctx);
        let bytes = scope.new_array(cratonvm_types::ArrayElementType::Byte, key_bytes.len());
        for (i, byte) in key_bytes.iter().enumerate() {
            scope.set_array_element(bytes, i, Value::Int(*byte as i8 as i32));
        }
        let bytes_h = scope.root(bytes);
        // The entry's OWN algorithm, not the constant "RAW" that used to be
        // written here: "RAW" is `SecretKeySpec.getFormat()`, and reporting the
        // format as the ALGORITHM broke every
        // `Cipher.getInstance(key.getAlgorithm())` on a key recovered from a
        // keystore.
        let alg_name = scope.create_string(algorithm);
        let alg_h = scope.root(alg_name);
        let key: ObjectRef =
            try_alloc_concurrent_synthetic(&mut *scope, "javax/crypto/spec/SecretKeySpec", 2)?;
        let bytes: ObjectRef = scope.get(&bytes_h);
        let alg_name: ObjectRef = scope.get(&alg_h);
        scope.set_field(key, 0, Value::Object(Some(bytes)));
        scope.set_field(key, 1, Value::Object(Some(alg_name)));
        Ok(Some(Value::Object(Some(key))))
    } else {
        Ok(Some(Value::Object(None)))
    }
}

/// Return the DER behind the compact four-slot `PrivateKey` proxy emitted by
/// [`engine_get_key`]. The proxy's final slot is a `(store_id, alias_hash)`
/// handle, not a fifth in-object DER field; `Key.getEncoded()` must resolve it
/// through the keystore registry instead of reading past the real interface
/// object's layout.
pub fn private_key_der_from_proxy(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
) -> Option<Vec<u8>> {
    if ctx.object_num_fields(key) <= 3 {
        return None;
    }
    let composite = match ctx.get_field(key, 3) {
        Value::Long(value) => value,
        _ => return None,
    };
    let alias_hash = composite as u32;
    // The other producer of this layout is
    // `x509_manager::make_private_key_mirror`, whose high half is a
    // `KeyManager` id from a DIFFERENT counter. It tags itself so the two are
    // distinguishable; untagged composites are keystore ids as before. Reading
    // a tagged one as a store id is what made
    // `KeyManager.getPrivateKey(alias).getEncoded()` return an empty array
    // whenever the two counters were not coincidentally aligned.
    if crate::x509_manager::is_km_proxy_composite(composite) {
        let km_id = crate::x509_manager::km_id_of_composite(composite);
        if km_id <= 0 {
            return None;
        }
        return crate::x509_manager::km_key_der_by_alias_hash(km_id, alias_hash);
    }
    let store_id = (composite >> 32) as i32;
    if store_id <= 0 {
        return None;
    }
    // Every `getEncoded()` / `Signature.initSign` on the proxy comes here:
    // only the one DER is copied out (gc-common w33-c, see `with_store`).
    with_store(store_id, |store| {
        store.entries.values().find_map(|entry| {
            if fnv1a_32(entry.alias.as_bytes()) != alias_hash {
                return None;
            }
            match &entry.kind {
                EntryKind::PrivateKey { key_der, .. } => Some(key_der.clone()),
                _ => None,
            }
        })
    })
    .flatten()
}

pub fn engine_get_certificate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;

    // Only the one DER is copied out (gc-common w33-c, see `with_store`).
    let cert_der = with_store(id, |store| match &store.entries.get(&alias)?.kind {
        EntryKind::TrustedCert { cert_der } => Some(cert_der.clone()),
        EntryKind::PrivateKey { chain, .. } => chain.first().cloned(),
        EntryKind::SecretKey { .. } => None,
    })
    .flatten();
    let Some(cert_der) = cert_der else {
        return Ok(Some(Value::Object(None)));
    };

    Ok(Some(Value::Object(Some(make_x509_mirror(
        ctx, &alias, &cert_der,
    )?))))
}

pub fn engine_get_certificate_chain(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;

    // Only the chain is copied out (gc-common w33-c, see `with_store`).
    let chain = with_store(id, |store| match &store.entries.get(&alias)?.kind {
        EntryKind::PrivateKey { chain, .. } => Some(chain.clone()),
        EntryKind::TrustedCert { .. } | EntryKind::SecretKey { .. } => None,
    })
    .flatten();
    let Some(chain) = chain else {
        return Ok(Some(Value::Object(None)));
    };

    let cls_id = match ctx.ensure_class_initialized("java/security/cert/X509Certificate") {
        Ok(c) => c,
        // Fallible since 2026-08-10 (Real-JDK wave 2, step 3): this is the
        // element class of the returned `Certificate[]`, and a fabricated
        // stand-in for a `java.security.cert` class is the compatibility
        // substitution contract §5 refuses. On a complete image the `Ok` arm is
        // what runs, so this changes nothing outside `--real-jdk` on a broken
        // image.
        Err(_) => {
            crate::util_concurrent_ext::refused_class(ctx, "java/security/cert/X509Certificate", 8)?
        }
    };
    let arr =
        crate::util_concurrent_ext::build_rooted_ref_array(ctx, cls_id, chain.len(), |ctx, i| {
            make_x509_mirror(ctx, &alias, &chain[i])
        })?;
    Ok(Some(Value::Object(Some(arr))))
}

pub fn engine_aliases(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);

    // Insertion order (`entries` is an `IndexMap`), NOT alphabetical — real
    // JDK's `KeyStore.aliases()` enumerates in the order entries were read
    // from the file (`LinkedHashMap`-backed), and Spring Boot's `SslInfo`
    // asserts on that exact positional order (see `LoadedKeyStore::entries`'s
    // doc comment). Only the aliases are copied out (gc-common w33-c).
    let aliases: Vec<String> =
        with_store(id, |store| store.entries.keys().cloned().collect()).unwrap_or_default();

    let cls_id = match ctx.ensure_class_initialized("java/lang/String") {
        Ok(c) => c,
        // Fallible since 2026-08-10 (Real-JDK wave 2, step 3). `java.lang.String`
        // is in every image, so the `Ok` arm is what runs; a run that reaches
        // this one has no `java.base` at all and fabricating a `String` stand-in
        // is not a recovery, it is a second failure wearing the first one's name.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "java/lang/String", 8)?,
    };
    let arr = crate::util_concurrent_ext::build_rooted_ref_array(
        ctx,
        cls_id,
        aliases.len(),
        |ctx, i| Ok(ctx.create_string(&aliases[i])),
    )?;

    // Under `--real-jdk` this fabrication is refused; fall back to an
    // enumeration the JDK builds itself (`Arrays$ArrayList` +
    // `Collections.enumeration`), the same landing `Enumeration$Impl` uses.
    // Without it the refusal surfaces as NoClassDefFoundError out of
    // `KeyStore.aliases()` and takes the whole `security` section of
    // `JdkOnlyPlatformProbe` with it. `Compatible` mode is unchanged: the
    // fallback is only reachable from the refusal arm.
    let arr_pin = ctx.pin_native_root(arr);
    let en = match try_alloc_concurrent_synthetic(ctx, "java/util/IteratorEnumeration", 2) {
        Ok(en) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(en, 0, Value::Object(Some(arr)));
            ctx.set_field(en, 1, Value::Int(0));
            Ok(en)
        }
        Err(refusal) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            match crate::classloader::real_snapshot_enumeration(ctx, arr) {
                Ok(Some(en)) => Ok(en),
                Ok(None) => Err(refusal),
                Err(err) => Err(err),
            }
        }
    };
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(en?))))
}

/// Public `KeyStore.aliases()` is intercepted by the early security shim in
/// real-JDK mode. Route that wrapper through the provider SPI's registry-backed
/// implementation rather than returning the legacy synthetic empty view.
pub fn keystore_aliases(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = unwrap_keystore_spi(ctx, this);
    let mut engine_args = args.to_vec();
    engine_args[0] = Value::Object(Some(spi));
    engine_aliases(ctx, &engine_args)
}

pub fn engine_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let n = with_store(id, |s| s.entries.len()).unwrap_or(0);
    Ok(Some(Value::Int(n as i32)))
}

/// Registry-backed counterpart for the public `KeyStore.size()` shim.
pub fn keystore_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = unwrap_keystore_spi(ctx, this);
    let mut engine_args = args.to_vec();
    engine_args[0] = Value::Object(Some(spi));
    engine_size(ctx, &engine_args)
}

pub fn engine_contains_alias(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let present = with_store(id, |s| s.entries.contains_key(&alias)).unwrap_or(false);
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

pub fn engine_is_key_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let yes = with_store(id, |s| {
        s.entries.get(&alias).map(|e| {
            matches!(
                e.kind,
                EntryKind::PrivateKey { .. } | EntryKind::SecretKey { .. }
            )
        })
    })
    .flatten()
    .unwrap_or(false);
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

pub fn engine_is_certificate_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let yes = with_store(id, |s| {
        s.entries
            .get(&alias)
            .map(|e| matches!(e.kind, EntryKind::TrustedCert { .. }))
    })
    .flatten()
    .unwrap_or(false);
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

fn engine_get_creation_date(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    // An ABSENT alias is null, not the epoch. `.unwrap_or(0)` handed back a
    // `Date` at time 0, so a caller testing `getCreationDate(a) != null` to ask
    // "does this entry exist" got true for every alias in existence.
    //
    // MEASURED in both modes (`probes/KeyStoreFamilySweep.java`):
    //
    //   getCreationDate("nope")
    //     HotSpot   null
    //     CratonVM  Wed Dec 31 21:00:00 UTC 1969
    //
    // Note the rendered date is the LOCAL-time face of epoch 0, which is what
    // made it read like a real timestamp rather than a default.
    let Some(ms) = with_store(id, |s| s.entries.get(&alias).map(|e| e.creation_time_ms)).flatten()
    else {
        return Ok(Some(Value::Object(None)));
    };

    // java/util/Date has a single `fastTime` long field in real-JDK layout
    // (slot 0 in our synthetic mirror).
    let date = try_alloc_concurrent_synthetic(ctx, "java/util/Date", 1)?;
    ctx.set_field(date, 0, Value::Long(ms));
    Ok(Some(Value::Object(Some(date))))
}

/// engineSetCertificateEntry(String alias, Certificate cert) — in-memory
/// mutation. The read-side natives (engineAliases/engineSize/engineGetCertificate)
/// are backed by the CV keystore side-table, but the real PKCS12KeyStore
/// bytecode for setCertificateEntry updates its own `entries` field, which the
/// natives never read — so `setCertificateEntry` was invisible to `aliases()`
/// (keycloak TruststoreBuilder: merged truststore reported 0 entries). Intercept
/// it and write the same side-table the reads consult. The cert DER comes from
/// [`certificate_der`] (real `Certificate.getEncoded()`, with a fallback for
/// this module's own synthetic X.509 mirror).
/// `KeyStoreSpi.engineGetCertificateAlias(Certificate)` — the first alias whose
/// stored certificate has the SAME ENCODING as the argument, or null.
///
/// MEASURED 2026-08-27 (`probes/KeyStoreFamilySweep.java`), both modes, and it
/// was FATAL rather than merely wrong:
///
/// ```text
/// KeyStore.getCertificateAlias(cert)
///   HotSpot   "mixedcasealias"
///   CratonVM  NullPointerException: Cannot invoke
///             "java.security.KeyStoreSpi.engineGetCertificateAlias(..)"
///             because "this.keystore" is null
///             at sun/security/util/KeyStoreDelegator.engineGetCertificateAlias
/// ```
///
/// The probe died at row 33 of 160, so 128 rows never ran.
///
/// Why it happened, which is the reusable part: this module registers SIXTEEN
/// `engine*` methods and this was the seventeenth. An unregistered one does not
/// fall back to something harmless — it reaches the REAL `KeyStoreDelegator`
/// bytecode, which dereferences `this.keystore`, a field the natives never
/// populate because this module keeps its state in its own side table. So the
/// cost of a missing registration in a half-shimmed class is a
/// `NullPointerException` from inside the JDK, not a missing feature. That is
/// the shape recorded at `a-shim-registered-for-a-descriptor-it-half-implements`.
///
/// Comparison is by DER, not by mirror identity: the argument is whatever
/// `Certificate` the caller holds, which in general is a different object from
/// the one `engineGetCertificate` would mint for the same entry.
pub fn engine_get_certificate_alias(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let Some(Value::Object(Some(cert))) = args.get(1) else {
        // The JDK's own body answers null for a null certificate rather than
        // throwing: `engineGetCertificateAlias` has no null check and the scan
        // simply matches nothing.
        return Ok(Some(Value::Object(None)));
    };
    let want = certificate_der(ctx, *cert);
    if want.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    // `entries` is an `IndexMap` (insertion order), so the scan order is
    // deterministic; the JDK's own order is its hash table's and is
    // unspecified, which is why the probe only ever puts ONE certificate in
    // the store before asking. Compared in place, the matching alias copied
    // out, and the String made after the lock is released (gc-common w33-c).
    let found = with_store(id, |store| {
        store.entries.iter().find_map(|(alias, entry)| {
            let have = match &entry.kind {
                EntryKind::TrustedCert { cert_der } => cert_der,
                EntryKind::PrivateKey { chain, .. } => chain.first()?,
                EntryKind::SecretKey { .. } => return None,
            };
            (*have == want).then(|| alias.clone())
        })
    })
    .flatten();
    match found {
        Some(alias) => Ok(Some(Value::Object(Some(ctx.create_string(&alias))))),
        None => Ok(Some(Value::Object(None))),
    }
}

pub fn engine_set_certificate_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let cert = match args.get(2) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(None),
    };
    // Pull the DER via the real Certificate.getEncoded() (with the synthetic
    // mirror fallback — see `certificate_der`).
    let der = certificate_der(ctx, cert);
    if der.is_empty() {
        // No encoding available — nothing to store (lenient; real JDK would
        // throw KeyStoreException, but a valid Certificate always encodes).
        return Ok(None);
    }
    keystore_set_cert_entry(id, &alias, der);
    Ok(None)
}

/// engineSetKeyEntry(String alias, Key key, char[] password, Certificate[]
/// chain) -- in-memory PrivateKey entry (companion to
/// engine_set_certificate_entry; see keystore_set_key_entry's doc comment
/// for why this matters). The key's PKCS#8 DER comes from the real
/// `PrivateKey.getEncoded()` (falling back to [`private_key_der_from_proxy`]
/// for this module's compact four-slot key proxy); each chain cert's DER from
/// [`certificate_der`], the same extraction engine_set_certificate_entry uses.
pub fn engine_set_key_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let key = match args.get(2) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(None),
    };

    // GC (gc-common w14-f): everything below asks Java -- `getFormat`,
    // `getEncoded`, `getAlgorithm`, each chain certificate's `getEncoded` --
    // and any of those can allocate and move the receiver, the key, the
    // password and the chain array this native used to hold raw across them.
    // What only needs reading is read NOW (the receiver's class, the
    // password); the key and the chain are pinned and re-read at each use.
    let supports_secret_keys = spi_supports_secret_keys(ctx, this);
    let pkcs12 = spi_wants_pkcs12_envelope(ctx, this);
    let entry_password = args
        .get(3)
        .map(|v| read_password(ctx, v))
        .unwrap_or_default();
    let key_pin = ctx.pin_native_root(key);
    let chain_pin = match args.get(4) {
        Some(Value::Object(Some(arr))) => Some((ctx.pin_native_root(*arr), *arr)),
        _ => None,
    };
    let result = set_key_entry_pinned(
        ctx,
        id,
        &alias,
        (key_pin, key),
        chain_pin,
        SetKeyEntryPolicy {
            supports_secret_keys,
            pkcs12,
            entry_password: &entry_password,
        },
    );
    ctx.unpin_native_roots(key_pin);
    result
}

/// What [`engine_set_key_entry`] reads off its receiver and password before
/// the first Java call.
struct SetKeyEntryPolicy<'a> {
    supports_secret_keys: bool,
    pkcs12: bool,
    entry_password: &'a [u8],
}

/// The body of [`engine_set_key_entry`], with the key and the chain array as
/// `(pin handle, original address)` pairs, re-read after every Java call.
fn set_key_entry_pinned(
    ctx: &mut dyn NativeContext,
    id: i32,
    alias: &str,
    key_pin: (usize, ObjectRef),
    chain_pin: Option<(usize, ObjectRef)>,
    policy: SetKeyEntryPolicy<'_>,
) -> MethodCallResult {
    let alias = alias.to_string();
    let key = ctx.read_native_pin(key_pin.0, key_pin.1);

    // `engineSetKeyEntry(String, Key, char[], Certificate[])` accepts ANY
    // `Key`. Real `PKCS12KeyStore` branches on `instanceof PrivateKey` vs
    // `instanceof SecretKey` and writes a SecretBag for the latter; this native
    // used to store every key as a `PrivateKey` entry, so
    // `setKeyEntry(a, new SecretKeySpec(raw, "DESede"), pw, null)` came back
    // out of `getKey` as a `PrivateKey` claiming algorithm RSA -- the raw bytes
    // survived, their type and algorithm did not.
    //
    // `Key.getFormat()` is the spec-defined discriminator ("RAW" for a secret
    // key, "PKCS#8" for a private key) and costs one virtual call, so ask it
    // rather than plumbing interface-identity checks through the native API.
    if string_from_virtual(ctx, key, "getFormat", "()Ljava/lang/String;").as_deref() == Some("RAW")
    {
        if !policy.supports_secret_keys {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/KeyStoreException",
                "Cannot store non-PrivateKeys",
            ));
        }
        let key = ctx.read_native_pin(key_pin.0, key_pin.1);
        let raw = read_encoded_byte_array(ctx, key);
        if raw.is_empty() {
            return Ok(None);
        }
        let key = ctx.read_native_pin(key_pin.0, key_pin.1);
        let key_algorithm = string_from_virtual(ctx, key, "getAlgorithm", "()Ljava/lang/String;")
            .unwrap_or_else(|| "AES".to_string());
        if let Err(failure) = require_encodable_secret_alg(ctx, &key_algorithm) {
            return Err(failure);
        }
        keystore_set_secret_key_entry(id, &alias, raw, &key_algorithm);
        return Ok(None);
    }

    let key = ctx.read_native_pin(key_pin.0, key_pin.1);
    let mut key_der = read_encoded_byte_array(ctx, key);
    if key_der.is_empty() {
        // `engine_get_key`'s compact four-slot `PrivateKey` proxy carries a
        // `(store_id, alias_hash)` handle instead of an in-object DER; when
        // `java/security/PrivateKey.getEncoded` is not the jca::key_factory
        // shim (which resolves that handle itself) the virtual call yields
        // nothing. Resolve the handle directly before giving up — otherwise
        // `ks2.setKeyEntry(a, ks1.getKey(a, pw), pw, ks1.getCertificateChain(a))`
        // (the standard keystore-merge idiom) silently dropped the entry.
        let key = ctx.read_native_pin(key_pin.0, key_pin.1);
        key_der = private_key_der_from_proxy(ctx, key).unwrap_or_default();
    }
    if key_der.is_empty() {
        // No PKCS#8 encoding available (e.g. a PKCS#11/HSM-backed key with
        // getEncoded() == null) -- nothing we can stage natively. Lenient,
        // matches the same leniency engine_set_certificate_entry takes.
        //
        // But not SILENT: an opaque key is the normal case for a delegating
        // provider (`MockAlternativeKeyProvider` in netty's
        // `JdkDelegatingPrivateKeyMethodTest`, any PKCS#11 or HSM key), and
        // dropping its entry with no trace means the alias simply is not there
        // later — indistinguishable from a keystore that was never populated.
        // the openssl-key-material-and-engine-residuals write-up (now retired)
        // asked for exactly this line. The store keeps working through the
        // live-keystore enumeration path
        // (`x509_manager::build_key_manager_state_from_live_keystore`), which
        // holds such a key BY REFERENCE, so this is a note about the NATIVE
        // staging only, not necessarily a broken handshake.
        let key = ctx.read_native_pin(key_pin.0, key_pin.1);
        let algorithm = string_from_virtual(ctx, key, "getAlgorithm", "()Ljava/lang/String;");
        let key = ctx.read_native_pin(key_pin.0, key_pin.1);
        let format = string_from_virtual(ctx, key, "getFormat", "()Ljava/lang/String;");
        eprintln!(
            "[keystore] setKeyEntry store_id={id} alias={alias:?} NOT staged natively: the key \
             reports no encoding (getEncoded() == null / empty), which is normal for an opaque \
             provider key. algorithm={:?} format={:?}; the live-KeyStore enumeration path keeps \
             it by reference instead.",
            algorithm, format,
        );
        return Ok(None);
    }
    let mut chain: Vec<Vec<u8>> = Vec::new();
    if let Some((chain_handle, chain_arr)) = chain_pin {
        let len = ctx.array_length(ctx.read_native_pin(chain_handle, chain_arr));
        for i in 0..len {
            // Re-read per element: `certificate_der` runs `getEncoded`.
            let arr = ctx.read_native_pin(chain_handle, chain_arr);
            if let Value::Object(Some(cert)) = ctx.get_array_element(arr, i) {
                let cv = certificate_der(ctx, cert);
                if !cv.is_empty() {
                    chain.push(cv);
                }
            }
        }
    }
    // FIX (httpserver-pkcs12-20260706): also install this as the process-wide
    // "runtime server identity" the native TLS listener consumes for the
    // actual accept-side handshake (io_native_tls's rustls ServerConfig),
    // exactly like engineLoad does for the byte-stream-load case. Without
    // this, a keystore built purely in-memory (getInstance() + load(null,
    // null) + setKeyEntry(...) -- Netty's JdkSslServerContext.buildKeyStore
    // ephemeral self-signed-cert pattern) staged an identity for
    // KeyManagerFactory/SSLContext's per-context bookkeeping (see
    // keystore_set_pending_km_identity's live-scan fallback above) but the
    // actual server socket had no certificate to present at all, and every
    // TLS handshake against it failed immediately ("unexpected EOF" on the
    // client side). See docs/feature-designs/native-dispatch-architecture.md.
    if crate::nbflags().dbg_tls_hs {
        eprintln!(
            "[dbg-tls-hs] engine_set_key_entry store_id={} alias={:?} key_len={} chain_len={} chain_cert_lens={:?}",
            id,
            alias,
            key_der.len(),
            chain.len(),
            chain.iter().map(|c| c.len()).collect::<Vec<_>>()
        );
    }
    if !chain.is_empty() {
        if crate::nbflags().dbg_tls_hs {
            eprintln!("[dbg-tls-hs] install_identity_from_der CALLER=engine_set_key_entry(direct-API) key_len={}", key_der.len());
        }
        crate::t27_tls::install_identity_from_der(ctx.vm_identity(), &key_der, &chain);
    }
    // KS-5: `engineSetKeyEntry(String, Key, char[], Certificate[])` -- args[3]
    // is the ENTRY password, and this native READ it nowhere. Wrap the key now,
    // in the format this store type writes, so `engineStore` has something to
    // emit that the entry password opens and `engineGetKey` has something to
    // check a password against.
    //
    // `key_der` itself stays plaintext: the TLS identity path
    // (`keystore_get_private_key`) reads it directly and must keep working for
    // an in-memory store that is never written out. The password and the
    // store type were read before the first Java call (see the caller).
    let protected = if policy.entry_password.is_empty() {
        None
    } else {
        protect_key_for_store(policy.pkcs12, &key_der, policy.entry_password)
    };
    keystore_set_key_entry(id, &alias, key_der, chain, protected);
    Ok(None)
}

/// PKCS#9 `friendlyName`, the attribute SunPKCS12 reports for every entry.
const PKCS9_FRIENDLY_NAME_OID: &str = "1.2.840.113549.1.9.20";
/// PKCS#9 `localKeyID`, which SunPKCS12 reports for every KEY entry.
const PKCS9_LOCAL_KEY_ID_OID: &str = "1.2.840.113549.1.9.21";
/// Oracle's `trustedKeyUsage`, which SunPKCS12 reports for a trusted
/// certificate entry; `write_pkcs12` writes it with the value below for every
/// one, so a store read back through this VM agrees with what it reports.
const PKCS12_TRUSTED_KEY_USAGE_OID: &str = "2.16.840.1.113894.746875.1.1";
/// `anyExtendedKeyUsage`, `setCertificateEntry`'s `trustedKeyUsage` value.
const ANY_EXTENDED_KEY_USAGE_OID: &str = "2.5.29.37.0";

/// The `(OID, value)` pairs SunPKCS12's `engineGetAttributes` reports for
/// `entry`, as `java.security.PKCS12Attribute(String, String)` arguments.
///
/// MEASURED on HotSpot 25.0.3 (`KeyStore.getInstance("PKCS12")`, `load(null,
/// null)`, then `setCertificateEntry("C", cert)` and `setKeyEntry("k", ...)`):
///
/// ```text
/// getAttributes("c")  [1.2.840.113549.1.9.20=C, 2.16.840.1.113894.746875.1.1=2.5.29.37.0]
/// getAttributes("k")  [1.2.840.113549.1.9.21=54:69:6d:65:20:31:37:39:...:38:39,
///                      1.2.840.113549.1.9.20=k]
/// ```
///
/// The `localKeyID` value is the UTF-8 of `"Time " + creationDate.getTime()`,
/// which is what SunPKCS12 mints for an entry it sets, rendered as the
/// colon-separated hex the `PKCS12Attribute` constructor parses back into an
/// OCTET STRING. Residuals, both recorded in the w34-c report: an entry READ
/// from a file reports a `localKeyID` derived the same way rather than the
/// file's own bytes (this module does not keep them), and the friendly name
/// is the alias as this module keys it -- lower case for an entry set through
/// the API, where SunPKCS12 keeps the caller's spelling ("C" above).
fn pkcs12_entry_attributes(entry: &KeyStoreEntry) -> Vec<(&'static str, String)> {
    let mut out = Vec::with_capacity(2);
    match &entry.kind {
        EntryKind::TrustedCert { .. } => {
            out.push((
                PKCS12_TRUSTED_KEY_USAGE_OID,
                ANY_EXTENDED_KEY_USAGE_OID.to_string(),
            ));
        }
        EntryKind::PrivateKey { .. } | EntryKind::SecretKey { .. } => {
            let key_id = format!("Time {}", entry.creation_time_ms);
            out.push((PKCS9_LOCAL_KEY_ID_OID, hex_colon_pairs(key_id.as_bytes())));
        }
    }
    // `PKCS12Attribute(String, String)` reads `value.charAt(0)`: an empty
    // value throws there, so an entry with an empty alias reports no name.
    if !entry.alias.is_empty() {
        out.push((PKCS9_FRIENDLY_NAME_OID, entry.alias.clone()));
    }
    out
}

/// `bytes` as lower-case, colon-separated hex pairs (`54:69:6d:65`), the form
/// `PKCS12Attribute` prints and parses for an OCTET STRING value.
fn hex_colon_pairs(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `Collections.emptySet()`: the default `KeyStoreSpi.engineGetAttributes`
/// answer, and SunPKCS12's for an alias it does not hold.
fn empty_attribute_set(ctx: &mut dyn NativeContext) -> MethodCallResult {
    ctx.invoke_static(
        "java/util/Collections",
        "emptySet",
        "()Ljava/util/Set;",
        &[],
    )
}

/// `KeyStoreSpi.engineGetAttributes(String)` (gc-common w34-c).
///
/// Unregistered, a `KeyStoreDelegator` receiver ran the real delegator body,
/// `this.keystore.engineGetAttributes(alias)`, on a field nothing populates:
/// `KeyStore.getAttributes` threw NullPointerException on every PKCS12 and JKS
/// keystore. HotSpot 25.0.3 answers `[]` for JKS and JCEKS (the
/// `KeyStoreSpi` default, `Collections.emptySet()`) and for an absent alias
/// in PKCS12, and an unmodifiable set of `PKCS12Attribute`s for a PKCS12 entry
/// (see [`pkcs12_entry_attributes`]). A null alias is NPE, as in HotSpot
/// (`KeyStore.getAttributes` checks it before the SPI is reached).
///
/// GC: `this` is read before the first allocation and not after; the set is
/// held in a handle scope across each attribute's construction and `add`.
fn engine_get_attributes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    if !spi_wants_pkcs12_envelope(ctx, this) {
        return empty_attribute_set(ctx);
    }
    let attrs = with_store(id, |s| s.entries.get(&alias).map(pkcs12_entry_attributes))
        .flatten()
        .unwrap_or_default();
    if attrs.is_empty() {
        return empty_attribute_set(ctx);
    }
    let mut scope = NativeHandleScope::new(ctx);
    let set = match scope.new_object_initialized("java/util/HashSet", "()V", &[])? {
        Some(Value::Object(Some(set))) => set,
        _ => return empty_attribute_set(&mut *scope),
    };
    let set_h = scope.root(set);
    for (oid, value) in &attrs {
        let name = scope.create_string(oid);
        let name_h = scope.root(name);
        let value = scope.create_string(value);
        let name = scope.get(&name_h);
        // `new_object_initialized` roots its arguments across the allocation.
        let attr = match scope.new_object_initialized(
            "java/security/PKCS12Attribute",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[Value::Object(Some(name)), Value::Object(Some(value))],
        )? {
            Some(Value::Object(Some(attr))) => attr,
            _ => continue,
        };
        let set = scope.get(&set_h);
        scope.invoke_virtual(
            set,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(attr))],
        )?;
    }
    let set = scope.get(&set_h);
    scope.invoke_static(
        "java/util/Collections",
        "unmodifiableSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        &[Value::Object(Some(set))],
    )
}

/// Is an entry of `kind` an instance of the `KeyStore.Entry` class named
/// `wanted` (internal form), as `engineEntryInstanceOf` answers it?
///
/// Two rules, both MEASURED on HotSpot 25.0.3. SunPKCS12 asks the entry's own
/// type. Every other store runs the `KeyStoreSpi` default:
/// `PrivateKeyEntry` is "a key entry with a certificate", `SecretKeyEntry`
/// "a key entry without one" -- so a private key set with no chain
/// (`setKeyEntry(a, byte[], null)`) is a `SecretKeyEntry` to JKS and JCEKS
/// and a `PrivateKeyEntry` to PKCS12, and this reproduces both. Any other
/// class, `KeyStore.Entry` itself included, is `false` (the JDK compares
/// class identity, and the three entry classes are final).
fn entry_is_instance_of(kind: &EntryKind, wanted: &str, pkcs12: bool) -> bool {
    let has_certificate = match kind {
        EntryKind::PrivateKey { chain, .. } => !chain.is_empty(),
        EntryKind::TrustedCert { .. } => true,
        EntryKind::SecretKey { .. } => false,
    };
    let is_key = !matches!(kind, EntryKind::TrustedCert { .. });
    match wanted {
        "java/security/KeyStore$TrustedCertificateEntry" => {
            matches!(kind, EntryKind::TrustedCert { .. })
        }
        "java/security/KeyStore$PrivateKeyEntry" if pkcs12 => {
            matches!(kind, EntryKind::PrivateKey { .. })
        }
        "java/security/KeyStore$PrivateKeyEntry" => is_key && has_certificate,
        "java/security/KeyStore$SecretKeyEntry" if pkcs12 => {
            matches!(kind, EntryKind::SecretKey { .. })
        }
        "java/security/KeyStore$SecretKeyEntry" => is_key && !has_certificate,
        _ => false,
    }
}

/// `KeyStoreSpi.engineEntryInstanceOf(String, Class)` (gc-common w34-c): the
/// real `KeyStoreDelegator` body dereferenced the unpopulated `this.keystore`
/// and threw NullPointerException. Answers from the side table by
/// [`entry_is_instance_of`]; an absent alias is `false`. Allocates nothing.
fn engine_entry_instance_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let wanted = match args.get(2) {
        Some(Value::Object(Some(mirror))) => ctx
            .class_id_from_mirror(*mirror)
            .and_then(|cid| ctx.class_name_of_id(cid)),
        _ => None,
    };
    let Some(wanted) = wanted else {
        return Ok(Some(Value::Int(0)));
    };
    let pkcs12 = spi_wants_pkcs12_envelope(ctx, this);
    let yes = with_store(id, |s| {
        s.entries
            .get(&alias)
            .map(|e| entry_is_instance_of(&e.kind, &wanted, pkcs12))
    })
    .flatten()
    .unwrap_or(false);
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

/// `KeyStoreSpi.engineSetKeyEntry(String, byte[], Certificate[])`, the
/// already-protected form (gc-common w34-c). The real `KeyStoreDelegator` body
/// dereferenced the unpopulated `this.keystore` (NullPointerException); on a
/// non-delegator SPI the real body wrote the SPI's own `entries`, which no
/// native here reads.
///
/// The bytes ARE the entry's envelope, stored as both the key and the entry's
/// own `protected` envelope: `engineGetKey` then opens it with the password it
/// is given (`keystore_unlock_private_keys`) and refuses a wrong one
/// (`UnrecoverableKeyException`), and `engineStore` writes it through
/// verbatim. Validation is HotSpot's, MEASURED on 25.0.3: JKS and PKCS12
/// refuse bytes that are not an `EncryptedPrivateKeyInfo` with a
/// `KeyStoreException` (these two messages), JCEKS accepts anything; a null
/// key is NPE. The chain is optional, as in the JDK.
///
/// GC: every read of `this` and of the key bytes happens before the first Java
/// call; the chain array is pinned across each certificate's `getEncoded`.
fn engine_set_protected_key_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let envelope = match args.get(2) {
        Some(Value::Object(Some(arr))) => read_byte_array(ctx, *arr),
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("the encoded parameter must be non-null".into()),
            }
            .into())
        }
    };
    let spi_class = class_name_of(ctx, this);
    if spi_class != JCEKS_FQN && !is_encrypted_private_key(&envelope) {
        let msg = if spi_class.starts_with("sun/security/pkcs12/") {
            "Private key is not stored as PKCS#8 EncryptedPrivateKeyInfo"
        } else {
            "key is not encoded as EncryptedPrivateKeyInfo"
        };
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/KeyStoreException",
            msg,
        ));
    }
    let mut chain: Vec<Vec<u8>> = Vec::new();
    if let Some(Value::Object(Some(arr))) = args.get(3) {
        let chain_arr = *arr;
        let chain_pin = ctx.pin_native_root(chain_arr);
        let current = ctx.read_native_pin(chain_pin, chain_arr);
        let len = ctx.array_length(current);
        for i in 0..len {
            // Re-read per element: `certificate_der` runs `getEncoded`.
            let current = ctx.read_native_pin(chain_pin, chain_arr);
            if let Value::Object(Some(cert)) = ctx.get_array_element(current, i) {
                let der = certificate_der(ctx, cert);
                if !der.is_empty() {
                    chain.push(der);
                }
            }
        }
        ctx.unpin_native_roots(chain_pin);
    }
    keystore_set_key_entry(id, &alias, envelope.clone(), chain, Some(envelope));
    Ok(None)
}

/// Refuse a secret-key algorithm this VM cannot encode into a SecretBag,
/// AT THE POINT THE ENTRY IS SET.
///
/// Real `PKCS12KeyStore.setKeyEntry` calls `AlgorithmId.get(algorithm)` while
/// storing and wraps its `NoSuchAlgorithmException` in a `KeyStoreException`,
/// so `setEntry` with e.g. a `"ChaCha20"` `SecretKeySpec` fails immediately on
/// HotSpot too. Deferring the complaint to `store()` would leave a caller
/// holding a keystore that looks fine and cannot be written — a second silent
/// no-op in place of the one this record is about.
fn require_encodable_secret_alg(
    ctx: &mut dyn NativeContext,
    algorithm: &str,
) -> Result<(), MethodCallFailed> {
    if secret_key_alg_oid(algorithm).is_some() {
        return Ok(());
    }
    Err(crate::phases_early::throw_jca_exc(
        ctx,
        "java/security/KeyStoreException",
        &format!(
            "Key protection algorithm not found: no PKCS#12 object identifier is known for \
             secret-key algorithm {algorithm:?}"
        ),
    ))
}

/// Invoke a no-argument virtual method returning a `String` and decode it.
fn string_from_virtual(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    name: &str,
    descriptor: &str,
) -> Option<String> {
    match ctx.invoke_virtual(obj, name, descriptor, &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// The exact class name of an object, or the empty string when it cannot be
/// resolved.
fn class_name_of(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    let class_id = ctx.class_id_of_object(obj);
    ctx.class_name_of_id(class_id).unwrap_or_default()
}

/// Whether the SPI this call landed on can hold a `SecretKeyEntry` at all.
///
/// PKCS#12 has a SecretBag; JKS has no representation for one, and real
/// `JavaKeyStore.engineSetKeyEntry` answers exactly
/// `KeyStoreException("Cannot store non-PrivateKeys")`. The check is by
/// receiver class rather than a flag threaded through `register_engine_surface`
/// because `NativeCallback` is a bare `fn` pointer and cannot capture the FQN
/// it was registered under.
fn spi_supports_secret_keys(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    !class_name_of(ctx, this).starts_with("sun/security/provider/JavaKeyStore")
}

/// Read `KeyStore.PasswordProtection.getPassword()` off a protection parameter.
/// `Ok(None)` means "not a `PasswordProtection`", which every caller has to
/// answer differently from "a `PasswordProtection` carrying a null password".
fn protection_password(
    ctx: &mut dyn NativeContext,
    prot: ObjectRef,
) -> Result<Option<Value>, MethodCallFailed> {
    if class_name_of(ctx, prot) != "java/security/KeyStore$PasswordProtection" {
        return Ok(None);
    }
    match ctx.invoke_virtual(prot, "getPassword", "()[C", &[])? {
        Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// engineSetEntry(String, KeyStore.Entry, KeyStore.ProtectionParameter).
///
/// WHY THIS HAS TO BE A NATIVE — this is the whole
/// `pkcs12-setentry-secretkeyentry-is-a-silent-noop-20260805` defect:
/// `KeyStore.setEntry` is real bytecode and delegates straight here, but real
/// `PKCS12KeyStore.engineSetEntry` does NOT route through the public
/// `engineSetKeyEntry`/`engineSetCertificateEntry` this module already
/// intercepts. It calls its own PRIVATE `setKeyEntry`/`setCertEntry`, which
/// mutate the SPI object's own `entries` map. Every read on this VM
/// (`engineAliases`, `engineSize`, `engineContainsAlias`, `engineGetKey`,
/// `engineStore`) is served from this file's side table instead, so the real
/// mutation landed somewhere nothing ever looks: `setEntry` returned normally,
/// threw nothing, and the entry was already gone before anything was
/// serialised. All three entry kinds were affected, not only `SecretKeyEntry`.
///
/// The decision tree below is `PKCS12KeyStore.engineSetEntry`'s, message for
/// message, including the two `KeyStoreException`s that are the only correct
/// answer for a missing password — a caller that checks for an exception has
/// to be told, and telling it is the entire point.
///
/// KNOWN LIMITATION, stated rather than hidden: the per-entry password is
/// accepted and validated but not retained; entries are held in the clear in
/// the side table and re-protected with the STORE password by `engineStore`.
/// That is the same limitation `jks_protect_key` documents for JKS, and it is
/// invisible to any caller that uses one password for both (every fixture, and
/// the overwhelmingly common case).
fn engine_set_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let alias = alias_arg(ctx, args)?;
    let entry = match args.get(2) {
        Some(Value::Object(Some(e))) => *e,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("KeyStore.setEntry: entry is null".into()),
            }
            .into())
        }
    };

    // GC (gc-common w14-f): `getPassword` and every entry getter below run
    // Java, which can allocate and move the receiver, the entry and the
    // password array this native used to hold raw across them. The receiver's
    // class is read now; the receiver and the entry are pinned and re-read;
    // the password is copied out as soon as it is returned.
    let supports_secret_keys = spi_supports_secret_keys(ctx, this);
    let pkcs12 = spi_wants_pkcs12_envelope(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    let entry_pin = ctx.pin_native_root(entry);
    let result = set_entry_pinned(
        ctx,
        args,
        &alias,
        (this_pin, this),
        (entry_pin, entry),
        (supports_secret_keys, pkcs12),
    );
    ctx.unpin_native_roots(this_pin);
    result
}

/// The body of [`engine_set_entry`], with the receiver and the entry as
/// `(pin handle, original address)` pairs; `spi` is the receiver's
/// `(supports secret keys, wants a PKCS#12 envelope)`.
fn set_entry_pinned(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    alias: &str,
    this_pin: (usize, ObjectRef),
    entry_pin: (usize, ObjectRef),
    spi: (bool, bool),
) -> MethodCallResult {
    let alias = alias.to_string();
    let (supports_secret_keys, pkcs12) = spi;
    // A protection parameter that is present but not a `PasswordProtection` is
    // rejected before anything is stored, exactly like the real SPI.
    let mut password: Option<Vec<u8>> = None;
    if let Some(Value::Object(Some(prot))) = args.get(3) {
        match protection_password(ctx, *prot)? {
            None => {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "unsupported protection parameter",
                ))
            }
            Some(Value::Object(None)) => {}
            // Copied before anything else runs Java.
            Some(v) => password = Some(read_password(ctx, &v)),
        }
    }

    // `this` IS the SPI here -- this is an `engine*` callback -- so the id is
    // read off the object the fifteen sibling callbacks read theirs off.
    let this = ctx.read_native_pin(this_pin.0, this_pin.1);
    let id = ensure_store_id_on(ctx, this);
    let entry = ctx.read_native_pin(entry_pin.0, entry_pin.1);
    let entry_class = class_name_of(ctx, entry);
    match entry_class.as_str() {
        "java/security/KeyStore$TrustedCertificateEntry" => {
            if password.is_some() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "trusted certificate entries are not password-protected",
                ));
            }
            let cert = match ctx.invoke_virtual(
                entry,
                "getTrustedCertificate",
                "()Ljava/security/cert/Certificate;",
                &[],
            )? {
                Some(Value::Object(Some(c))) => c,
                _ => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/security/KeyStoreException",
                        &format!("setEntry({alias:?}): entry carries no trusted certificate"),
                    ))
                }
            };
            let der = certificate_der(ctx, cert);
            if der.is_empty() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    &format!(
                        "setEntry({alias:?}): no DER encoding available for this certificate \
                         — nothing was stored"
                    ),
                ));
            }
            keystore_set_cert_entry(id, &alias, der);
            Ok(None)
        }
        "java/security/KeyStore$PrivateKeyEntry" => {
            if password.is_none() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "non-null password required to create PrivateKeyEntry",
                ));
            }
            let key = match ctx.invoke_virtual(
                entry,
                "getPrivateKey",
                "()Ljava/security/PrivateKey;",
                &[],
            )? {
                Some(Value::Object(Some(k))) => k,
                _ => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/security/KeyStoreException",
                        &format!("setEntry({alias:?}): entry carries no private key"),
                    ))
                }
            };
            // Pinned with the rest (released by the caller's unpin): its
            // `getEncoded` runs Java and the proxy fallback reads its fields.
            let key_pin = ctx.pin_native_root(key);
            let mut key_der = read_encoded_byte_array(ctx, key);
            if key_der.is_empty() {
                // Same handle-resolution fallback `engine_set_key_entry` needs:
                // this module's own compact `PrivateKey` proxy carries a
                // `(store_id, alias_hash)` pair instead of an in-object DER.
                let key = ctx.read_native_pin(key_pin, key);
                key_der = private_key_der_from_proxy(ctx, key).unwrap_or_default();
            }
            if key_der.is_empty() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    &format!(
                        "setEntry({alias:?}): the private key has no PKCS#8 encoding this VM \
                         can store (getEncoded() returned nothing) — nothing was stored"
                    ),
                ));
            }
            let mut chain: Vec<Vec<u8>> = Vec::new();
            let entry = ctx.read_native_pin(entry_pin.0, entry_pin.1);
            if let Some(Value::Object(Some(arr))) = ctx.invoke_virtual(
                entry,
                "getCertificateChain",
                "()[Ljava/security/cert/Certificate;",
                &[],
            )? {
                let arr_pin = ctx.pin_native_root(arr);
                let len = ctx.array_length(arr);
                for i in 0..len {
                    // Re-read per element: `certificate_der` runs `getEncoded`.
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    if let Value::Object(Some(cert)) = ctx.get_array_element(arr, i) {
                        let der = certificate_der(ctx, cert);
                        if !der.is_empty() {
                            chain.push(der);
                        }
                    }
                }
            }
            // Same TLS staging `engine_set_key_entry` performs: an in-memory
            // keystore built through setEntry is just as much a server identity
            // as one built through setKeyEntry.
            if !chain.is_empty() {
                crate::t27_tls::install_identity_from_der(ctx.vm_identity(), &key_der, &chain);
            }
            // KS-5, the `setEntry` door onto the same store. `password` here is
            // the `PasswordProtection`'s char[], copied out above.
            let protected = match password.as_ref() {
                Some(pw) => {
                    if pw.is_empty() {
                        None
                    } else {
                        protect_key_for_store(pkcs12, &key_der, pw)
                    }
                }
                None => None,
            };
            keystore_set_key_entry(id, &alias, key_der, chain, protected);
            Ok(None)
        }
        "java/security/KeyStore$SecretKeyEntry" => {
            if password.is_none() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "non-null password required to create SecretKeyEntry",
                ));
            }
            if !supports_secret_keys {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "Cannot store non-PrivateKeys",
                ));
            }
            let key = match ctx.invoke_virtual(
                entry,
                "getSecretKey",
                "()Ljavax/crypto/SecretKey;",
                &[],
            )? {
                Some(Value::Object(Some(k))) => k,
                _ => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/security/KeyStoreException",
                        &format!("setEntry({alias:?}): entry carries no secret key"),
                    ))
                }
            };
            // Pinned with the rest: `getEncoded` runs Java before `getAlgorithm`.
            let key_pin = ctx.pin_native_root(key);
            let raw = read_encoded_byte_array(ctx, key);
            if raw.is_empty() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    &format!(
                        "setEntry({alias:?}): the secret key has no RAW encoding this VM can \
                         store (getEncoded() returned nothing) — nothing was stored"
                    ),
                ));
            }
            let key = ctx.read_native_pin(key_pin, key);
            let algorithm = string_from_virtual(ctx, key, "getAlgorithm", "()Ljava/lang/String;")
                .unwrap_or_else(|| "AES".to_string());
            require_encodable_secret_alg(ctx, &algorithm)?;
            keystore_set_secret_key_entry(id, &alias, raw, &algorithm);
            Ok(None)
        }
        other => {
            let dotted = other.replace('/', ".");
            Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/KeyStoreException",
                &format!("unsupported entry type: {dotted}"),
            ))
        }
    }
}

/// engineGetEntry(String, KeyStore.ProtectionParameter).
///
/// The companion to [`engine_set_entry`], and needed for the same reason: real
/// `PKCS12KeyStore.engineGetEntry` reads the SPI object's own `entries` map,
/// which on this VM is empty because every mutation lands in this file's side
/// table. Leaving it to real bytecode meant `getEntry` answered `null` for an
/// alias that `containsAlias`/`aliases()`/`getKey` all reported as present, and
/// `UnrecoverableKeyException` for a trusted-cert entry that
/// `setCertificateEntry` had just stored successfully.
///
/// The algorithm is `KeyStoreSpi.engineGetEntry`'s, which is defined purely in
/// terms of the `engine*` accessors this module already owns.
fn engine_get_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;

    // Only the entry's shape is read (gc-common w33-c, see `with_store`).
    let shape = with_store(id, |store| {
        let entry = store.entries.get(&alias)?;
        let chain_len = match &entry.kind {
            EntryKind::PrivateKey { chain, .. } => chain.len(),
            _ => 0,
        };
        Some((
            matches!(entry.kind, EntryKind::TrustedCert { .. }),
            chain_len,
            matches!(entry.kind, EntryKind::SecretKey { .. }),
        ))
    })
    .flatten();
    let Some((is_cert_entry, chain_len, is_secret)) = shape else {
        return Ok(Some(Value::Object(None)));
    };

    let prot = match args.get(2) {
        Some(Value::Object(Some(p))) => Some(*p),
        _ => None,
    };

    let Some(prot) = prot else {
        if !is_cert_entry {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/UnrecoverableKeyException",
                "requested entry requires a password",
            ));
        }
        let cert = engine_get_certificate(ctx, args)?;
        let cert = match cert {
            Some(v @ Value::Object(Some(_))) => v,
            _ => return Ok(Some(Value::Object(None))),
        };
        return ctx.new_object_initialized(
            "java/security/KeyStore$TrustedCertificateEntry",
            "(Ljava/security/cert/Certificate;)V",
            &[cert],
        );
    };

    // GC (gc-common w14-f): `getPassword` runs Java, and so do the key and
    // chain builders below, each of which allocates. The receiver and the
    // alias string were re-read from `args` after them, and the key object
    // held across the chain's allocation, so a moving collection in between
    // looked the entry up through a stale receiver or handed the
    // `PrivateKeyEntry` constructor a stale key. Pinned, and re-read at each
    // use; released together.
    let this_pin = ctx.pin_native_root(this);
    let alias_pin = match args.get(1) {
        Some(Value::Object(Some(a))) => Some((ctx.pin_native_root(*a), *a)),
        _ => None,
    };
    let result = get_protected_entry(
        ctx,
        &alias,
        (this_pin, this),
        alias_pin,
        ProtectedEntryShape {
            prot,
            is_cert_entry,
            is_secret,
            chain_len,
        },
    );
    ctx.unpin_native_roots(this_pin);
    result
}

/// What [`engine_get_entry`] learned about the entry before its first Java
/// call.
struct ProtectedEntryShape {
    prot: ObjectRef,
    is_cert_entry: bool,
    is_secret: bool,
    chain_len: usize,
}

/// The password-protected half of [`engine_get_entry`], with the receiver and
/// the alias string as `(pin handle, original address)` pairs.
fn get_protected_entry(
    ctx: &mut dyn NativeContext,
    alias: &str,
    this_pin: (usize, ObjectRef),
    alias_pin: Option<(usize, ObjectRef)>,
    shape: ProtectedEntryShape,
) -> MethodCallResult {
    let ProtectedEntryShape {
        prot,
        is_cert_entry,
        is_secret,
        chain_len,
    } = shape;
    let alias = alias.to_string();
    // The receiver and alias as `args` carried them, re-read now.
    let fresh_args = |ctx: &dyn NativeContext| -> [Value; 2] {
        [
            Value::Object(Some(ctx.read_native_pin(this_pin.0, this_pin.1))),
            match alias_pin {
                Some((h, a)) => Value::Object(Some(ctx.read_native_pin(h, a))),
                None => Value::Object(None),
            },
        ]
    };

    let Some(password) = protection_password(ctx, prot)? else {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/UnsupportedOperationException",
            "unsupported protection parameter",
        ));
    };
    if is_cert_entry {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/UnsupportedOperationException",
            "trusted certificate entries are not password-protected",
        ));
    }

    let [this_now, alias_now] = fresh_args(&*ctx);
    // `password` was just returned by `getPassword`: current.
    let key_args = [this_now, alias_now, password];
    let key = match engine_get_key(ctx, &key_args)? {
        Some(v @ Value::Object(Some(_))) => v,
        _ => return Ok(Some(Value::Object(None))),
    };

    if is_secret {
        return ctx.new_object_initialized(
            "java/security/KeyStore$SecretKeyEntry",
            "(Ljavax/crypto/SecretKey;)V",
            &[key],
        );
    }

    if chain_len == 0 {
        // Unreachable through any API real JDK permits — `PKCS12KeyStore`
        // refuses to store a private key without a chain, and
        // `PrivateKeyEntry`'s constructor rejects a zero-length one. Say so
        // instead of letting the constructor throw an
        // `IllegalArgumentException` that names neither the alias nor the
        // keystore.
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/KeyStoreException",
            &format!(
                "getEntry({alias:?}): this private-key entry has no certificate chain, so no \
                 PrivateKeyEntry can be built for it"
            ),
        ));
    }
    // The key is held across the chain's allocation.
    let Value::Object(Some(key_obj)) = key else {
        return Ok(Some(Value::Object(None)));
    };
    let key_pin = ctx.pin_native_root(key_obj);
    let chain_args = fresh_args(&*ctx);
    let chain = match engine_get_certificate_chain(ctx, &chain_args)? {
        Some(v @ Value::Object(Some(_))) => v,
        _ => Value::Object(None),
    };
    let key = Value::Object(Some(ctx.read_native_pin(key_pin, key_obj)));
    ctx.new_object_initialized(
        "java/security/KeyStore$PrivateKeyEntry",
        "(Ljava/security/PrivateKey;[Ljava/security/cert/Certificate;)V",
        &[key, chain],
    )
}

/// engineDeleteEntry(String alias) -- in-memory removal (companion to
/// engine_set_certificate_entry; same side-table consistency rationale).
pub fn engine_delete_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    keystore_delete_entry(id, &alias);
    Ok(None)
}

/// engineStore(OutputStream, char[]) — serialise the side-table to the JKS wire
/// format and write it to the stream. The read natives are backed by the CV
/// side-table; real PKCS12KeyStore.engineStore would serialise its own (empty)
/// `entries` field, so a round-trip (store → load → aliases) reported 0 entries
/// (keycloak TruststoreBuilderTest.testMergedTrustStore). Writing JKS (which
/// CV's load path detects by magic and parses via load_jks) round-trips the
/// entries through CV regardless of the declared KeyStore type.
/// Hand `bytes` to a Java `OutputStream` via `write(byte[])`.
///
/// Extracted so the three format arms of [`engine_store`] share one emitter
/// rather than three copies of the array-building loop.
fn write_bytes_to_stream(
    ctx: &mut dyn NativeContext,
    out: ObjectRef,
    bytes: &[u8],
) -> MethodCallResult {
    // gc-common w20-e: `write` is dispatched on the stream's address after
    // the array's allocation, not its entry one.
    let out_pin = ctx.pin_native_root(out);
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    let out = ctx.read_native_pin(out_pin, out);
    ctx.unpin_native_roots(out_pin);
    ctx.invoke_virtual(out, "write", "([B)V", &[Value::Object(Some(arr))])?;
    Ok(None)
}

pub fn engine_store(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let out = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // JKS DEMANDS a store password; PKCS#12 permits a null one.
    //
    // MEASURED in both modes (`probes/KeyStoreFamilySweep.java`), and it is one
    // of the places the module's own header claim -- "PKCS12 + JKS share the
    // same `engine*` surface" -- is false:
    //
    //   ks.store(out, null)
    //     JKS      HotSpot IllegalArgumentException   CratonVM no-throw
    //     PKCS12   HotSpot no-throw                   CratonVM no-throw
    //
    // Real `JavaKeyStore.engineStore` opens with `if (password == null) throw
    // new IllegalArgumentException("password can't be null")`; `PKCS12KeyStore`
    // has no such line and treats null as "no encryption of the MAC". Keyed on
    // the receiver class for the same reason `spi_supports_secret_keys` is --
    // `NativeCallback` is a bare `fn` pointer and cannot capture the FQN it was
    // registered under.
    //
    // Note this is NOT the same axis as the format choice below: that one is
    // decided by CONTENT (a SecretKeyEntry forces PKCS#12), while the refusal
    // is decided by the SPI the caller actually asked for.
    if matches!(args.get(2), None | Some(Value::Object(None)))
        && class_name_of(ctx, this).starts_with("sun/security/provider/JavaKeyStore")
    {
        return Err(RuntimeError::IllegalArgumentException {
            message: "password can't be null".to_string(),
        }
        .into());
    }
    let password = match args.get(2) {
        Some(v) => read_password(ctx, v),
        None => Vec::new(),
    };
    let store = keystore_lookup(id).unwrap_or_default();
    // Format choice, stated plainly: JKS cannot represent a `SecretKeyEntry`
    // at all, so a store holding one has to go out as PKCS#12 or lose the
    // entry -- which is exactly what this did, silently, until now. Stores
    // WITHOUT a secret key keep the byte-for-byte JKS body, because that is
    // the shape every already-validated round trip in this VM (the keycloak
    // truststore merge, the TLS identity paths) is measured against, and CV's
    // loader detects either format by magic on the way back in.
    // FORMAT BY DECLARED TYPE FIRST. The SPI class the caller actually got is
    // right here -- the refusal above already reads it -- and it is the only
    // thing that says which format the caller ASKED for.
    //
    // This used to be decided by CONTENT alone, with the reason stated: "CV's
    // loader detects either format by magic on the way back in". That is true
    // of CV's loader, and it is the whole premise: it assumes nothing else will
    // ever read the file. So `KeyStore.getInstance("PKCS12").store(..)` emitted
    // a JKS file -- first four bytes `feedfeed` where HotSpot writes a DER
    // SEQUENCE -- and every PKCS12 keystore this VM produced was unreadable by
    // keytool, OpenSSL and every other JVM. MEASURED by
    // `KeyStoreTypeProbe.java`.
    //
    // `write_pkcs12` already existed and was already under test; it simply was
    // not reachable from the declared type.
    let spi = class_name_of(ctx, this);
    if spi.starts_with("sun/security/pkcs12/PKCS12KeyStore") {
        let bytes = write_pkcs12(&store, &password).map_err(|message| {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IOException { message },
            ))
        })?;
        return write_bytes_to_stream(ctx, out, &bytes);
    }
    if spi.starts_with("com/sun/crypto/provider/JceKeyStore") {
        let bytes = write_jks_with_magic(&store, &password, JCEKS_MAGIC);
        return write_bytes_to_stream(ctx, out, &bytes);
    }
    let bytes = if store
        .entries
        .values()
        .any(|e| matches!(e.kind, EntryKind::SecretKey { .. }))
    {
        match write_pkcs12(&store, &password) {
            Ok(bytes) => bytes,
            Err(message) => {
                // Never fall back to JKS here: that would drop the secret key
                // and hand the caller a valid-looking file, which is the
                // original defect.
                return Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(RuntimeError::IOException { message }),
                ));
            }
        }
    } else {
        write_jks(&store, &password)
    };
    write_bytes_to_stream(ctx, out, &bytes)
}

// ---------------------------------------------------------------------------
// Helpers — store-id stash, mirror allocation, alias decode
// ---------------------------------------------------------------------------

/// Extract the alias `String` argument of an `engine*` callback.
///
/// The alias is the second arg (`args.get(1)`) of every alias-taking
/// `engine*` method (`engineGetKey`, `engineGetCertificate`, …). It arrives
/// as a `Value::Object(Some(string_ref))`; we decode it through the
/// `NativeContext::read_string` accessor the rest of `native-builtins` uses
/// (e.g. `log4j_extras::extract_logger_name`). Returns `None` for a null /
/// non-string arg so the caller can `.unwrap_or_default()` to the empty
/// alias, matching real-JDK's behaviour of treating a null alias as "no such
/// entry".
fn read_string_arg(ctx: &mut dyn NativeContext, v: &Value) -> Option<String> {
    match v {
        Value::Object(Some(o)) => ctx.read_string(*o),
        _ => None,
    }
}

/// Copy a Java `byte[]` out of the heap. Returns an empty vec for anything
/// that is not a non-empty array.
pub fn read_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push(b as u8);
        }
    }
    out
}

/// Invoke `getEncoded()[B` on `obj` and copy the result out. Empty vec when the
/// call fails, returns null, or returns a zero-length array.
pub fn read_encoded_byte_array(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Vec<u8> {
    match ctx.invoke_virtual(obj, "getEncoded", "()[B", &[]) {
        Ok(Some(Value::Object(Some(arr)))) => read_byte_array(ctx, arr),
        _ => Vec::new(),
    }
}

/// DER encoding of a `java.security.cert.Certificate`.
///
/// Prefers the real `Certificate.getEncoded()`. Falls back to the raw DER this
/// module itself stashes in slot 3 of the synthetic
/// `java/security/cert/X509Certificate` mirror ([`make_x509_mirror`]): in
/// synthetic-JDK mode the registered `X509Certificate.getEncoded()` shim
/// (`phases_late::ssl_security`) hands back an EMPTY array unless the
/// `legacy-synthetic-crypto` cert store is compiled in, so the virtual call
/// alone loses the very bytes we put there. Without the fallback,
/// `setCertificateEntry` on a mirror-backed certificate stored nothing and
/// `aliases()`/`size()` reported an empty keystore.
///
/// The fallback is deliberately narrow — it fires only for objects whose exact
/// class is the synthetic mirror — so it can never misread an unrelated field
/// of a real `sun.security.x509.X509CertImpl`.
pub fn certificate_der(ctx: &mut dyn NativeContext, cert: ObjectRef) -> Vec<u8> {
    let der = read_encoded_byte_array(ctx, cert);
    if !der.is_empty() {
        return der;
    }
    if !matches!(ctx.heap_kind_of(cert), cratonvm_types::ObjectKind::Object) {
        return Vec::new();
    }
    if ctx.object_num_fields(cert) <= 3 {
        return Vec::new();
    }
    let cls_id = ctx.class_id_of_object(cert);
    match ctx.class_name_of_id(cls_id) {
        Some(name) if name == "java/security/cert/X509Certificate" => {
            match ctx.get_field(cert, 3) {
                Value::Object(Some(arr)) => read_byte_array(ctx, arr),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Get this keystore's `KEYSTORE_REGISTRY` id, allocating and stamping an empty
/// store when it has none yet.
///
/// `get_store_id` answers 0 for a keystore that was never `engineLoad`ed, and
/// every mutator (`keystore_set_cert_entry`, …) silently no-ops on id 0. The
/// synthetic-JDK `java.security.KeyStore` in `tls.rs` needs a store the moment
/// its first entry is set, so give it one on demand; `set_store_id` records the
/// id on the object's slot/named field AND in the identity side-table, so every
/// later read resolves the same store.
pub fn keystore_ensure_store_id(ctx: &mut dyn NativeContext, ks_obj: ObjectRef) -> i32 {
    let target = unwrap_keystore_spi(ctx, ks_obj);
    ensure_store_id_on(ctx, target)
}

/// [`keystore_ensure_store_id`] without the unwrap, for a caller that already
/// holds the SPI.
///
/// Every other `engine*` callback in this file reads its id with a plain
/// `get_store_id(ctx, this)`; `engine_set_entry` was the one that went through
/// the unwrapping form, and on a `KeyStoreDelegator` receiver that is how it
/// came to write its entries somewhere no reader looks. Asking for the id of
/// the object you were handed is the invariant the other fifteen callbacks
/// hold, so it is spelled out here rather than left to the unwrap being
/// harmless.
fn ensure_store_id_on(ctx: &mut dyn NativeContext, target: ObjectRef) -> i32 {
    let existing = get_store_id(ctx, target);
    if existing != 0 {
        return existing;
    }
    let id = keystore_register_in_vm(ctx.vm_identity(), LoadedKeyStore::default());
    set_store_id(ctx, target, id);
    id
}

/// True when `id`'s store currently holds `alias`. Used by the public
/// `KeyStore` shim to verify a mutation actually landed before reporting
/// success (a dropped entry must surface as `KeyStoreException`, not silence).
pub fn keystore_has_alias(id: i32, alias: &str) -> bool {
    registry()
        .read()
        .stores
        .get(&id)
        .map(|s| s.entries.contains_key(alias))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Public `java.security.KeyStore` shims
//
// The public `KeyStore` wrapper is intercepted by `tls.rs` (synthetic-JDK mode)
// and `phases_early.rs`. Both must reach THIS module's registry-backed engine
// surface rather than reimplementing storage, otherwise the public API and the
// SPI disagree about what the keystore contains. Each shim unwraps a real
// `keyStoreSpi` delegate when there is one (so we drive the same object
// `engineLoad` stamped) and otherwise operates on the wrapper itself — the
// synthetic `KeyStore` has no SPI and IS its own store. Same shape as
// `keystore_aliases`/`keystore_size` above.
// ---------------------------------------------------------------------------

/// Rewrite `args[0]` to the SPI delegate (or the receiver itself) and hand the
/// call to one of the `engine_*` implementations.
fn via_spi(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    engine: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = unwrap_keystore_spi(ctx, this);
    let mut engine_args = args.to_vec();
    engine_args[0] = Value::Object(Some(spi));
    engine(ctx, &engine_args)
}

/// `KeyStore.load(InputStream, char[])` → `engineLoad`.
pub fn keystore_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    via_spi(ctx, args, engine_load)
}

/// `KeyStore.getKey(String, char[])` → `engineGetKey`.
pub fn keystore_get_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    via_spi(ctx, args, engine_get_key)
}

/// `KeyStore.getCertificate(String)` → `engineGetCertificate`.
pub fn keystore_get_certificate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_get_certificate)
}

/// `KeyStore.getCertificateChain(String)` → `engineGetCertificateChain`.
pub fn keystore_get_certificate_chain(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_get_certificate_chain)
}

/// `KeyStore.containsAlias(String)` → `engineContainsAlias`.
pub fn keystore_contains_alias(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_contains_alias)
}

/// `KeyStore.isKeyEntry(String)` → `engineIsKeyEntry`.
pub fn keystore_is_key_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_is_key_entry)
}

/// `KeyStore.isCertificateEntry(String)` → `engineIsCertificateEntry`.
pub fn keystore_is_certificate_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_is_certificate_entry)
}

/// `KeyStore.setCertificateEntry(String, Certificate)` → the SPI mutator.
/// (`_native` suffix: the plain names belong to this module's registry-level
/// `keystore_set_cert_entry` / `keystore_set_key_entry` helpers.)
pub fn keystore_set_certificate_entry_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_set_certificate_entry)
}

/// `KeyStore.setKeyEntry(String, Key, char[], Certificate[])` → the SPI mutator.
pub fn keystore_set_key_entry_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_set_key_entry)
}

/// `KeyStore.deleteEntry(String)` → `engineDeleteEntry`.
pub fn keystore_delete_entry_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_delete_entry)
}

/// `KeyStore.store(OutputStream, char[])` → `engineStore` (the JKS writer).
pub fn keystore_store(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    via_spi(ctx, args, engine_store)
}

pub fn make_x509_mirror(
    ctx: &mut dyn NativeContext,
    alias: &str,
    cert_der: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    // Build the DER byte[] once — used either as the ctor arg for the real cert
    // or stashed in the synthetic-mirror fallback.
    let arr = ctx.new_array(ArrayElementType::Byte, cert_der.len());
    for (i, b) in cert_der.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    // Prefer a REAL `sun.security.x509.X509CertImpl` parsed from the DER. A bare
    // synthetic `java/security/cert/X509Certificate` is the ABSTRACT class, so
    // the SunX509 KeyManager's chain validation — `cert.checkValidity(Date)`
    // during init — throws `AbstractMethodError` ("no Code attribute"). The real
    // X509CertImpl implements the full X509Certificate API (checkValidity,
    // getSubjectX500Principal, getPublicKey, getEncoded, …) via real bytecode.
    if let Ok(Some(Value::Object(Some(real)))) = ctx.new_object_initialized(
        "sun/security/x509/X509CertImpl",
        "([B)V",
        &[Value::Object(Some(arr))],
    ) {
        return Ok(real);
    }
    // Fallback: synthetic mirror (subject/issuer = alias, DER in slot 3). Reached
    // only if the real DER parse fails (e.g. an unimplemented DerValue native).
    let cert_obj = try_alloc_concurrent_synthetic(ctx, "java/security/cert/X509Certificate", 4)?;
    // `arr` and `cert_obj` are both live across `create_string` below, which
    // allocates and can therefore relocate either of them under a moving young
    // collection — pin both and re-read through the pins. Same Family-1 shape
    // as the chain-array fill in `t27_tls::engine_run_trust_check`; a store
    // through a stale ref is silently DROPPED by the heap guard, which here
    // would leave the mirror with a null DER and no subject/issuer.
    let arr_pin = ctx.pin_native_root(arr);
    let cert_pin = ctx.pin_native_root(cert_obj);
    // Field layout matches what `phases_early.rs` uses for the
    // `getCertificate` path: 0=subject string, 1=issuer string, 2=cert_id,
    // 3=DER (byte[]). Subject + issuer here are alias strings — the real
    // X.509 CN extraction lives in `security_manager/x509.rs`, which TLS
    // can call once it has the DER. We keep the alias as a stand-in so
    // tests asserting on `getName()` see something stable.
    let alias_str = ctx.create_string(alias);
    let cert_obj = ctx.read_native_pin(cert_pin, cert_obj);
    ctx.set_field(cert_obj, 0, Value::Object(Some(alias_str)));
    ctx.set_field(cert_obj, 1, Value::Object(Some(alias_str)));
    ctx.set_field(cert_obj, 2, Value::Int(0));

    // Stash the DER (reuse the byte[] built above) so consumers can call
    // `Certificate.getEncoded()` or pass the bytes to a TLS/`X509TrustManager`.
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(cert_obj, 3, Value::Object(Some(arr)));
    ctx.unpin_native_roots(arr_pin);
    Ok(cert_obj)
}

/// Identity-keyed fallback for the store-id, used when the KeyStoreSpi object
/// has no field to hold it. A real `sun.security.provider.JavaKeyStore$JKS` has
/// a SINGLE instance field (`entries`), so neither the synthetic
/// `cratonvm$keystore$storeId` field (which doesn't exist on the real class →
/// `set_field_by_name` no-ops) nor slot `FIELD_STORE_ID=4` (out of range) can
/// store it — `engineAliases` then looked up store 0 and reported an empty
/// keystore, so KeyManagerFactory found "No aliases for private keys" and TLS
/// init failed. `identity_hash_code` is stable across GC, so key on it.
///
/// The key is `(NativeContext::vm_identity(), identity_hash_code(spi))`, not a
/// bare identity hash: a hash code is unique only *within one heap* while this
/// table is a process-global `static`. Rust tests (and any embedder) create
/// several independent `Vm`s in one process, so a bare-`i32` key let VM B's
/// KeyStoreSpi resolve to VM A's store id and read another VM's keys and
/// certificates out of `registry()`. Same rule as
/// `crypto_impl::RSA_REALKEY_MAP` / `jca::signature::SigKey`
/// (`native-api/src/registry.rs`, `vm_identity` doc). Values are plain `i32`
/// store ids -- no `ObjectRef`s, so no collector scan/remap companion is
/// needed. (`store_identity_pem_map` below needs no VM component: its key is a
/// `registry()` store id drawn from a single process-wide counter, so it is
/// already globally unique.)
///
/// Keyed by the object's WEAK LOCK KEY since gc-common w27-b
/// (`common-w26b-tls-sibling-tables-keyed-by-vm-folded-identity-hash`), no
/// longer `(vm, identity hash)`: the identity hash is 32 bits from one
/// per-heap counter that wraps, so two LIVE `KeyStoreSpi`s of one VM could
/// share a row and read each other's keys and certificates. See
/// [`store_row_key`]. A row goes when the lock-key sweep frees its key
/// ([`forget_keystore_keys`]); no row is read after its object's death. The
/// VM is kept in the row for teardown.
#[allow(clippy::type_complexity)]
fn store_id_by_identity() -> &'static std::sync::Mutex<std::collections::HashMap<usize, StoreIdRow>>
{
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, StoreIdRow>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// One [`store_id_by_identity`] row: the store id and the VM that filed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StoreIdRow {
    vm: usize,
    id: i32,
}

/// `obj`'s identity-tier store id, if it has a row. Never mints a key
/// (`crate::existing_weak_lock_key`): an object never keyed has no row.
fn lookup_store_id_row(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<i32> {
    let key = crate::existing_weak_lock_key(ctx, obj)?;
    store_id_by_identity()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|row| row.id)
}

/// File `obj`'s identity-tier store id (`obj` CURRENT). No row for an object
/// with no identity hash, as before gc-common w27-b.
fn file_store_id_row(ctx: &dyn NativeContext, obj: ObjectRef, id: i32) {
    let Some(key) = store_row_key(ctx, obj) else {
        return;
    };
    let vm = ctx.vm_identity();
    store_id_by_identity()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, StoreIdRow { vm, id });
}

/// `store_id` → (cert_pem, key_pem) for the keystore's first key entry. Recorded
/// at `engineLoad`; consumed by `keystore_set_pending_km_identity` (called from
/// `KeyManagerFactory.init`) to drive the per-`SSLContext` mTLS identity flow in
/// `t27_tls`.
#[allow(clippy::type_complexity)]
fn store_identity_pem_map(
) -> &'static std::sync::Mutex<std::collections::HashMap<i32, (String, String)>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<i32, (String, String)>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Unwrap a `java.security.KeyStore` wrapper down to its real `KeyStoreSpi`
/// instance (the private `keyStoreSpi` field, real KeyStore's 3rd of 3 fields:
/// `type`, `provider`, `keyStoreSpi`). `KeyManagerFactory.init(KeyStore,
/// char[])` (and `TrustManagerFactory.init(KeyStore)`) receive the outer
/// wrapper, but `engineLoad`/`engineSetKeyEntry`/etc. run natively on the SPI
/// object itself (registered on the SPI's own class name) and stamp the
/// store-id side-table keyed off the SPI's identity -- calling
/// `get_store_id` directly on the wrapper looks at the WRONG object (the
/// wrapper's own fields/identity hash have nothing to do with the SPI's), so
/// it always read back id=0 and every staged identity was silently dropped.
/// Falls back to the wrapper object itself if the field can't be read (e.g.
/// a non-standard `KeyStore` subclass), preserving old behaviour for any
/// caller that doesn't hit this real-bytecode shape.
fn unwrap_keystore_spi(ctx: &mut dyn NativeContext, keystore_obj: ObjectRef) -> ObjectRef {
    if let Value::Object(Some(spi)) = ctx.get_field_by_name(keystore_obj, "keyStoreSpi") {
        return spi;
    }
    // Fallback: real KeyStore's field order is type(0)/provider(1)/keyStoreSpi(2).
    //
    // THE INDEX IS BLIND, SO WHAT IT PRODUCES IS CHECKED BEFORE IT IS BELIEVED.
    // This helper is also reached with a receiver that is ALREADY an SPI --
    // `keystore_ensure_store_id` is called from `engine_set_entry`, whose
    // `this` is the provider's own `KeyStoreSpi`. `KeyStore.getInstance("PKCS12")`
    // resolves to `sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12`, a
    // `sun/security/util/KeyStoreDelegator` whose slot 2 is `primaryKeyStore`:
    // a `java.lang.Class` MIRROR, not a keystore. The name lookup above misses
    // (a delegator has no `keyStoreSpi` field), the index fired, and every
    // `setEntry` was stored against the store id of a process-global class
    // mirror -- which no reader ever asks. `setEntry` returned normally,
    // `size()`/`aliases()`/`getKey()` answered as if nothing had been stored,
    // and `store()` serialised an empty keystore. Measured on JDK 25, both
    // modes, `apps/probes/JdkOnlyPlatformProbe.java`'s `security` section.
    //
    // So the candidate must actually BE a `KeyStoreSpi`. When that class
    // cannot be resolved at all the historical blind behaviour is kept rather
    // than silently changed on an image that has no such class.
    if ctx.object_num_fields(keystore_obj) > 2 {
        if let Value::Object(Some(spi)) = ctx.get_field(keystore_obj, 2) {
            let Some(spi_cid) = ctx.class_id_by_name("java/security/KeyStoreSpi") else {
                return spi;
            };
            let actual = ctx.class_id_of_object(spi);
            if actual == spi_cid || ctx.is_subclass(actual, spi_cid) {
                return spi;
            }
        }
    }
    keystore_obj
}

/// `KeyManagerFactory.init(KeyStore, char[])` calls this so the keystore's
/// identity is staged for the next `SSLContext.init` on this thread (see
/// `t27_tls::set_pending_km_identity`).
pub fn keystore_set_pending_km_identity(
    ctx: &mut dyn NativeContext,
    keystore_obj: ObjectRef,
) {
    keystore_set_pending_km_identity_with_password(ctx, keystore_obj, &[]);
}

/// Same as [`keystore_set_pending_km_identity`], with the KeyManagerFactory's
/// per-entry password. JKS permits a key password distinct from the store-load
/// password; Spring/Tomcat use that arrangement for their `test.jks` fixture.
pub fn keystore_set_pending_km_identity_with_password(
    ctx: &mut dyn NativeContext,
    keystore_obj: ObjectRef,
    password: &[u8],
) {
    let spi = unwrap_keystore_spi(ctx, keystore_obj);
    let id = get_store_id(ctx, spi);
    if id == 0 {
        return;
    }
    keystore_unlock_private_keys(ctx.vm_identity(), id, password);
    if password.is_empty() {
        let ident = store_identity_pem_map().lock().unwrap().get(&id).cloned();
        if let Some((cert, key)) = ident {
            if crate::nbflags().dbg_tls_hs {
                eprintln!(
                    "[dbg-tls-hs] keystore_set_pending_km_identity_with_password store_id={} SOURCE=cached-snapshot cert_pem_len={} key_pem_len={}",
                    id, cert.len(), key.len()
                );
            }
            crate::t27_tls::set_pending_km_identity(cert, key);
            return;
        }
    }
    // FIX (httpserver-pkcs12-20260706): store_identity_pem_map is a
    // load-time snapshot (populated only by engineLoad's entries scan). A
    // keystore built via getInstance()+load(null,null)+setKeyEntry(...) --
    // Netty's JdkSslServerContext.buildKeyStore ephemeral self-signed-cert
    // pattern -- has an empty snapshot at that id (load(null,null) sees zero
    // entries) even though setKeyEntry has since added a real PrivateKey
    // entry via keystore_set_key_entry. Fall back to scanning the LIVE
    // registry (keystore_get_first_private_key) so this identity isn't
    // silently dropped -- without this, SSLContext.init produced a
    // server-side context with no certificate to present and the TLS
    // handshake failed immediately.
    if let Some((key_der, chain)) = keystore_get_first_private_key(id) {
        let (cert_pem, key_pem) = crate::t27_tls::der_identity_to_pem(&key_der, &chain);
        store_identity_pem_map()
            .lock()
            .unwrap()
            .insert(id, (cert_pem.clone(), key_pem.clone()));
        crate::t27_tls::set_pending_km_identity(cert_pem, key_pem);
    }
}

/// Materialize JKS private-key entries when their per-entry password becomes
/// available. `jks_recover_key` authenticates the plaintext, so it is safe to
/// attempt this on PKCS#8/P12 entries too: non-JKS data simply remains intact.
fn keystore_unlock_private_keys(vm: usize, id: i32, password: &[u8]) {
    if password.is_empty() {
        return;
    }
    let mut stores = registry().write();
    let Some(store) = stores.stores.get_mut(&id) else {
        return;
    };
    let mut installed_identity = false;
    for entry in store.entries.values_mut() {
        if let EntryKind::PrivateKey {
            key_der,
            chain,
            protected,
        } = &mut entry.kind
        {
            // Both schemes, since KS-6: a PKCS#12 bag the store password did
            // not open is still sitting here as its own envelope.
            if let Some(plain) = recover_key_any_scheme(key_der, password) {
                // gc-common w34-c discovery: KEEP the envelope as the entry's
                // own. It used to be overwritten and lost, so after ONE
                // correct `getKey` every later `getKey` succeeded with ANY
                // password (nothing was left to check it against), and
                // `store()` re-protected the key under the STORE password,
                // changing the entry's password on the round trip. MEASURED
                // on HotSpot 25.0.3 (JKS, PKCS12, JCEKS; key password "kp",
                // store password "pw"): getKey kp -> key, then "wrong" and
                // "pw" -> UnrecoverableKeyException; stored again and
                // reloaded, kp -> key and pw -> UnrecoverableKeyException.
                // `engine_get_key` checks `protected`, and both writers emit
                // it verbatim. An entry that already has its own envelope
                // (set through the API) keeps that one.
                let envelope = std::mem::replace(key_der, plain);
                if protected.is_none() {
                    *protected = Some(envelope);
                }
                if !installed_identity {
                    if crate::nbflags().dbg_tls_hs {
                        eprintln!(
                            "[dbg-tls-hs] install_identity_from_der CALLER=keystore_unlock_private_keys(store_id={}) key_len={}",
                            id,
                            key_der.len()
                        );
                    }
                    crate::t27_tls::install_identity_from_der(vm, key_der, chain);
                    installed_identity = true;
                }
            }
        }
    }
}
/// Fetch the PKCS#8 DER + cert chain of the first `PrivateKey` entry in a
/// registered store, scanning the LIVE registry rather than the load-time
/// snapshot `store_identity_pem_map` relies on. See
/// `keystore_set_pending_km_identity`'s fallback for why this is needed.
fn keystore_get_first_private_key(id: i32) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let store = registry().read().stores.get(&id).cloned()?;
    if crate::nbflags().dbg_tls_hs {
        let summary: Vec<String> = store
            .entries
            .iter()
            .map(|(alias, e)| match &e.kind {
                EntryKind::PrivateKey { key_der, chain, .. } => format!(
                    "{alias}=PrivateKey(key_len={},chain_cert_lens={:?})",
                    key_der.len(),
                    chain.iter().map(|c| c.len()).collect::<Vec<_>>()
                ),
                EntryKind::TrustedCert { cert_der } => {
                    format!("{alias}=TrustedCert(len={})", cert_der.len())
                }
                EntryKind::SecretKey { .. } => format!("{alias}=SecretKey"),
            })
            .collect();
        eprintln!(
            "[dbg-tls-hs] keystore_get_first_private_key store_id={} entries={:?}",
            id, summary
        );
    }
    for (alias, entry) in store.entries.iter() {
        if let EntryKind::PrivateKey { key_der, chain, .. } = &entry.kind {
            if crate::nbflags().dbg_tls_hs {
                eprintln!(
                    "[dbg-tls-hs] keystore_get_first_private_key store_id={} PICKED alias={:?}",
                    id, alias
                );
            }
            return Some((key_der.clone(), chain.clone()));
        }
    }
    None
}

fn get_store_id(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    // Try multiple storage strategies; the synthetic class allocation pattern
    // means our `engine*` callbacks may run on either a real-JDK PKCS12KeyStore
    // mirror (with all the real fields) or a 5-field synthetic mirror that
    // `tls.rs` allocates. Probe by name first, fall back to the conventional
    // slot index, then the identity side-table.
    let by_name = ctx.get_field_by_name(this, "cratonvm$keystore$storeId");
    if let Value::Int(i) = by_name {
        if i != 0 {
            return i;
        }
    }
    if let Value::Long(l) = by_name {
        if l != 0 {
            return l as i32;
        }
    }
    // The slot tier, on the synthetic layout only (gc-common w34-c): on a real
    // class slot 4 is a real field, and an `int` one would be misread as an id.
    if store_id_slot_is_synthetic(&*ctx, this) {
        match ctx.get_field(this, FIELD_STORE_ID) {
            Value::Int(i) if i != 0 => return i,
            Value::Long(l) if l != 0 => return l as i32,
            _ => {}
        }
    }
    // Identity side-table fallback (real JKS objects have no field for it).
    // Keyed by the object's weak lock key -- see `store_id_by_identity`'s doc
    // comment.
    lookup_store_id_row(&*ctx, this).unwrap_or(0)
}

/// FIX (tls-residuals): the robust, three-tier id lookup `tls.rs`'s own doc
/// comment on `read_keystore_registry_id` asked for ("a `pub fn
/// keystore::keystore_id_from_object` ... that reuses `get_store_id`").
/// `x509_manager::read_keystore_id`/`tls::read_keystore_registry_id` each
/// re-implemented a SUBSET of `get_store_id`'s lookup (named field + a slot
/// index) but never the identity-hash side-table tier — the ONLY tier that
/// resolves a real `java.security.KeyStore` object in real-JDK mode, since
/// its real 4-field layout (`type`/`provider`/`keyStoreSpi`/`initialized`)
/// has no room for a pseudo-field and `set_field_by_name` no-ops on a real
/// class. Without this, `TrustManagerFactory.init(KeyStore)` (both the
/// SimpleFactory and PKIXFactory SPI paths) always resolved id 0 for a
/// caller-supplied truststore, silently falling back to the ~100+ platform
/// root certs instead of the caller's actual (possibly test-only/private) CA
/// — the client then rejects ANY peer certificate signed by that CA with
/// `UnknownIssuer`, regardless of any other TLS/OCSP configuration.
pub fn keystore_id_from_object(ctx: &mut dyn NativeContext, ks_obj: ObjectRef) -> i32 {
    let id = get_store_id(ctx, ks_obj);
    if id != 0 {
        return id;
    }
    // `engineLoad` (this file) is registered on the KeyStoreSpi delegate
    // class, so `set_store_id`'s identity-hash tier stamps the SPI
    // instance's identity — a DIFFERENT object from the public
    // `java.security.KeyStore` wrapper callers like
    // `TrustManagerFactory.init(KeyStore)` actually receive. Re-run the same
    // lookup against the wrapper's `keyStoreSpi` delegate field so that tier
    // has a chance to match.
    if let Value::Object(Some(spi)) = ctx.get_field_by_name(ks_obj, "keyStoreSpi") {
        return get_store_id(ctx, spi);
    }
    0
}

/// Stamp store `id` on `this`, which must be the CURRENT address (the id is
/// written through it, and it is recorded as the store's carrier).
fn set_store_id(ctx: &mut dyn NativeContext, this: ObjectRef, id: i32) {
    // gc-common w14-f: an object re-stamped with another id (a load that
    // could not replace its old store in place) stops carrying the old one.
    let previous = get_store_id(ctx, this);
    ctx.set_field_by_name(this, "cratonvm$keystore$storeId", Value::Int(id));
    // Only into the synthetic layout's slot: on a real `KeyStoreDelegator` /
    // `PKCS12KeyStore` slot 4 is a real field, which this used to null
    // (gc-common w34-c, see `store_id_slot_is_synthetic`).
    if store_id_slot_is_synthetic(&*ctx, this) {
        ctx.set_field(this, FIELD_STORE_ID, Value::Int(id));
    }
    // Always record in the identity side-table so retrieval works even when the
    // KeyStoreSpi object has no usable field (real JavaKeyStore$JKS = 1 field).
    // Filed under `this`'s weak lock key (current: the id was just written
    // through it), so the row goes with `this` and no other object -- a later
    // one, or a LIVE one with the same identity hash -- ever resolves it
    // (gc-common w13-a, w27-b).
    file_store_id_row(&*ctx, this, id);
    // The store lives while `this` does (gc-common w14-f). An object with no
    // identity hash pins it instead.
    note_store_carrier(&*ctx, this, id);
    if previous != 0 && previous != id {
        forget_store_carrier(&*ctx, this, previous);
    }
}

/// Map the first byte of a PKCS#8 DER to a coarse algorithm index. RSA and
/// EC are the only two we expect from a TLS keystore. Default to RSA-ish.
fn detect_algo_idx(key_der: &[u8]) -> i32 {
    // PKCS#8 PrivateKeyInfo: SEQUENCE { Integer 0, AlgorithmIdentifier, OCTET STRING }
    // AlgorithmIdentifier: SEQUENCE { OID, optional params }. RSA OID is
    // 1.2.840.113549.1.1.1 (DER `06 09 2A 86 48 86 F7 0D 01 01 01`).
    // EC OID is `1.2.840.10045.2.1` (`06 07 2A 86 48 CE 3D 02 01`).
    let needle_rsa = [
        0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01,
    ];
    let needle_ec = [0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
    if find_subseq(key_der, &needle_rsa).is_some() {
        return 6; // RSA
    }
    if find_subseq(key_der, &needle_ec).is_some() {
        return 7; // EC
    }
    6 // default RSA
}

fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

fn fnv1a_32(b: &[u8]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &x in b {
        h ^= x as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

/// `unwrap_keystore_spi`'s slot-2 fallback, both directions.
///
/// # The defect these hold shut
///
/// `KeyStore.setEntry("secret", new SecretKeyEntry(...), new PasswordProtection(pw))`
/// returned normally and stored NOTHING. Measured on JDK 25 in both
/// `--fast-boot` and `--real-jdk`:
///
/// ```text
///                          HotSpot   CratonVM
/// size() after setEntry       1         0
/// containsAlias("secret")   true     false
/// getKey("secret", pw)     <key>      null
/// store(...) byte count      421       122      (an EMPTY keystore)
/// setKeyEntry(...)   -- the pre-1.5 route to the same thing -- worked
/// ```
///
/// `engine_set_entry` was the one `engine*` callback that read its store id
/// through `keystore_ensure_store_id`, which UNWRAPS. Its receiver is already
/// the SPI, and for `KeyStore.getInstance("PKCS12")` that SPI is
/// `sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12`, a
/// `sun/security/util/KeyStoreDelegator`. A delegator has no `keyStoreSpi`
/// field, so the name lookup missed and the blind slot-2 fallback fired --
/// and slot 2 of `KeyStoreDelegator` is `primaryKeyStore`, a `java.lang.Class`
/// MIRROR. Every `setEntry` was therefore filed under the store id of a
/// process-global class mirror, which no reader ever asks for.
///
/// # Why a mock and not a source scan
///
/// A source witness would pin today's spelling of the guard; these call the
/// function. The second test is the one that keeps the first honest: a guard
/// that rejected everything would make the delegator case pass while silently
/// breaking every real `java.security.KeyStore` wrapper, which is the caller
/// the fallback exists for.
#[cfg(test)]
mod unwrap_keystore_spi_tests;

/// gc-common w11-a (`common-w10e-tls-manager-and-keystore-registries-grow-for-
/// the-life-of-a-vm`): a reload replaces its own VM's store in place and never
/// another VM's.
#[cfg(test)]
mod w11a_keystore_reload_tests;

/// gc-common w14-f: a store lives while something can still ask for it, and
/// goes with its last carrier (`common-w10e-tls-manager-and-keystore-
/// registries-grow-for-the-life-of-a-vm`). Every test has its own VM identity
/// and tears it down in a `Drop` guard; the collector is modelled by
/// `t27_tls::gc_sweep_tls_rows` and then `crate::gc_sweep_lock_keys` (the
/// order of all three GC sites; the carrier rows go with their weak lock keys
/// since gc-common w27-b) with an explicit live set.
#[cfg(test)]
mod w14f_store_lifetime_tests;

/// gc-common w27-b (`common-w26b-tls-sibling-tables-keyed-by-vm-folded-identity-hash`):
/// the identity-tier store id rows and the carrier rows are keyed by the
/// object's weak lock key, so two LIVE objects of one VM that share an
/// identity hash keep separate rows, a freed key takes its rows (and releases
/// the stores it was the last carrier of), and two VMs stay apart.
///
/// The mock's identity hash is the address truncated to `i32`, so addresses
/// 4 GiB apart share it. The helpers under test touch only the identity hash
/// and the VM identity, so bare addresses stand in for objects. Private VM
/// identities; the guard forgets only those.
#[cfg(test)]
mod w27b_store_row_key_tests;

/// gc-common w33-c (`common-w25-keystore-pkcs12-setentry-rarely-reads-back-
/// unequal-on-zgc`): an unauthenticated envelope opened with a WRONG password
/// used to "succeed" whenever the noise it decrypted to happened to end in
/// valid PKCS#7 padding (~1/255 per attempt, two attempts per bag). Pure
/// functions, no VM: nothing here touches a process-wide table.
#[cfg(test)]
mod w33c_pkcs12_false_open_tests;

/// gc-common w34-c: `common-w33c-keystore-store-id-slot-overlays-a-real-spi-field-FIXED-20260923`.
///
/// Section 1: the store-id slot tier writes and reads slot 4 only on the
/// synthetic layout, never a real field. Section 2: the three
/// `KeyStoreDelegator` methods that NPE'd are registered as bridges and
/// answer from the side table. Plus the creation-date discovery.
#[cfg(test)]
mod w34c_store_id_slot_and_delegator_tests;

#[cfg(test)]
mod compat_real_keystore_spi_tests;
