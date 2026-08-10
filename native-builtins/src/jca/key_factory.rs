// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.4 + WP6.6 — `java.security.KeyPairGenerator`, `KeyPair`, `KeyFactory`,
//! `PublicKey` / `PrivateKey` accessors. Real-JDK mode.
//!
//! ## Why duplicate `crypto.rs::register_key_pair_generator`?
//!
//! `crypto.rs` is gated behind `legacy-synthetic-crypto` AND is only wired
//! into `register_synthetic_overrides` — i.e. it never runs in default
//! real-JDK mode.  Real-JDK `KeyPairGenerator.getInstance` therefore falls
//! through to the JDK 25 bytecode which calls
//! `sun.security.jca.GetInstance.getServices(...)` and NPEs because we
//! don't materialize a real `Provider.services` map.
//!
//! Registering native overrides for the public surface (`getInstance`,
//! `initialize`, `generateKeyPair`, `KeyPair.getPublic` / `getPrivate`,
//! `Key.getAlgorithm` / `getEncoded` / `getFormat`, plus `KeyFactory`)
//! short-circuits the bytecode path entirely.  We back the real RSA + EC
//! key generation with the `crypto_impl` software primitives that already
//! power the synthetic-mode crypto path.
//!
//! ## Object layout
//!
//! Both `KeyPairGenerator` and `KeyFactory` are 3-field synthetics:
//!
//! | Slot | Field          |
//! |------|----------------|
//! |  0   | `algo_idx` Int |
//! |  1   | `key_size` Int |
//! |  2   | `state`    Int |  (0=created, 1=initialized)
//!
//! `PublicKey` / `PrivateKey` are 4-field synthetics that match the
//! existing `crypto.rs` shape, so the `Signature` natives in
//! `jca::signature` can read `key_id` from slot 3:
//!
//! | Slot | Field          |
//! |------|----------------|
//! |  0   | `algo_idx`  Int|
//! |  1   | `size_bits` Int|
//! |  2   | `enc_len`   Int|
//! |  3   | `key_id`    Long  (0 = no real key, otherwise crypto_impl handle) |
//!
//! ## Algorithm indexing
//!
//! Aligned with `crypto.rs::KPG_ALGORITHMS`:
//! `0=ML-KEM-512, 1=ML-KEM-768, 2=ML-KEM-1024, 3=ML-DSA-44, 4=ML-DSA-65,
//!  5=ML-DSA-87, 6=RSA, 7=EC, 8=Ed25519, 9=X25519`.
//!
//! Wave 6 in scope: 6 (RSA) and 7 (EC).  The other indices are still
//! recognised so the synthetic Ed25519 / ML-* probes don't regress, but
//! their `generateKeyPair` only touches the synthetic key shape.

#![allow(clippy::collapsible_if)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

use crate::try_alloc_concurrent_synthetic;
use crate::crypto_impl;

const ALGO_RSA: i32 = 6;
const ALGO_EC: i32 = 7;
const ALGO_ED25519: i32 = 8;
const ALGO_ED448: i32 = 10;
// CratonVM has no synthetic DSA key material at all (unlike RSA/EC, which
// have a fast synthetic path with real-key routing layered on top) — DSA
// always routes to the real `sun.security.provider.DSAKeyFactory` SPI (see
// `kf_generate_public`/`kf_generate_private`). Found root-causing
// `SecurityInfoTests.getWhenJarIsSigned`: `X509Key.parse()` (real bytecode,
// reached while re-parsing a real X.509 cert's SubjectPublicKeyInfo) calls
// `KeyFactory.getInstance("DSA").generatePublic(x509KeySpec)`, which — before
// this fix — fell through to the generic/unrecognized-algorithm synthetic
// path (`algo_idx("DSA")` was unmapped, returning -1) and failed with
// `InvalidKeySpecException: cannot generate a usable Unknown public key`.
const ALGO_DSA: i32 = 11;
const ALGO_X25519: i32 = 9;
const ALGO_X448: i32 = 12;
// Sentinel algo indices for the GENERIC `KeyFactory` names `"XDH"` and
// `"EdDSA"` — unlike `ALGO_X25519`/`ALGO_X448`/`ALGO_ED25519`/`ALGO_ED448`
// (curve pinned by the requested algorithm string itself), these carry no
// curve information: the real JDK's non-nested `sun.security.ec.XDHKeyFactory`
// / `sun.security.ec.ed.EdDSAKeyFactory` determine X25519-vs-X448 /
// Ed25519-vs-Ed448 by sniffing the `AlgorithmIdentifier` OID embedded in the
// spec's own encoded bytes at `generatePublic`/`generatePrivate` time (see
// `resolve_curve_algo`). `PemPrivateKeyParser` (Spring Boot's PEM SSL bundle
// loader) always requests the generic names — never the curve-specific ones —
// so without this sentinel + sniff, `KeyFactory.getInstance("XDH"/"EdDSA")`
// fell through unmapped (XDH) or was silently misrouted to Ed25519 regardless
// of the key's real curve (EdDSA), breaking Ed448/X448/X25519 PEM
// private-key parsing. Used only by `kf_algo_idx` (KeyFactory) — the shared
// `algo_idx` (also used by `KeyPairGenerator`, which has no such generic
// name to resolve) is untouched.
const ALGO_XDH_GENERIC: i32 = 13;
const ALGO_EDDSA_GENERIC: i32 = 14;
// `KeyFactory.getInstance("RSASSA-PSS")` is a DISTINCT, STRICTER real SPI
// (`sun.security.rsa.RSAKeyFactory$PSS`) from the permissive `"RSA"` /
// `RSAKeyFactory$Legacy` that `ALGO_RSA` already drives: `$Legacy` REJECTS a
// PKCS#8 key whose `AlgorithmIdentifier` OID is `id-RSASSA-PSS`
// (`InvalidKeyException: Expected a RSA key, but got RSASSA-PSS`, verified
// against real JDK 25), while `$PSS` requires exactly that OID.
// `PemPrivateKeyParser`'s per-algorithm fallback loop retries with the
// literal name `"RSASSA-PSS"` after the `"RSA"`-named attempt throws (see
// `PemPrivateKeyParser.PemParser.parse`); routing that through the shared,
// KeyPairGenerator-facing `algo_idx` (which deliberately collapses
// `"RSASSA-PSS"` onto `ALGO_RSA` — the PSS choice belongs to `Signature`, not
// key generation) would hit the exact same `$Legacy` rejection twice and
// never reach a working factory, so this is `kf_algo_idx`-only too.
const ALGO_RSASSA_PSS: i32 = 15;

// ---------------------------------------------------------------------------
// Real-JDK class instance-field counts (number of slots used by the real
// layout before our synthetic state begins). All our private state slots
// are placed *after* the real layout so the VM's descriptor-aware
// `set_field`/`get_field` doesn't coerce our `Int(...)` writes to
// `Object(None)` on slots that the real JDK class declares as references.
//
// JDK 25:
//   * `java.security.KeyPairGenerator` extends `KeyPairGeneratorSpi`:
//        - KPGSpi: 0 instance fields
//        - KPG: `algorithm: String`, `provider: Provider` -> 2
//   * `java.security.KeyFactory`:
//        - 5 instance fields (algorithm, provider, spi, lock, serviceIterator)
//   * `java.security.KeyPair`:
//        - 2 fields (privateKey, publicKey) - both Object, order doesn't
//          matter as long as our accessors match what set_field writes via
//          the same indices, so we ignore the JDK ordering here and use
//          slot 0 = pub / slot 1 = priv as our internal convention.
//
// We resolve the actual field counts at runtime via `class_num_total_fields`
// instead of hard-coding so this stays robust against future JDK layout
// changes. The `kpg_base_offset` helper returns the first usable slot index
// past the real layout.
fn synthetic_base_offset(ctx: &mut dyn NativeContext, class_name: &str) -> usize {
    let cid = ctx
        .ensure_class_initialized(class_name)
        .unwrap_or(ClassId::new(0));
    ctx.class_num_total_fields(cid)
}

// ---------------------------------------------------------------------------
// SigProbe fix: process-wide side tables for KPG / KeyFactory algorithm +
// key size.  Real-JDK class layouts make raw-slot writes of `Value::Int`
// silently turn into `Value::Object(None)` (the inherited slot 0 is an
// object reference, not an int), so the raw-slot path is unreliable across
// the `getInstance` → `initialize` → `generateKeyPair` chain.  Side tables
// keyed on the receiver `ObjectRef` survive layout changes — same proven
// pattern as `message_digest::accumulators`.  The base-offset slot writes
// below are kept as a secondary store for synthetic-mode callers.
// ---------------------------------------------------------------------------

// GC-stable side-table key (cce0079 follow-up): these tables were keyed by
// the raw `ObjectRef` ADDRESS. A moving young GC that relocates a live
// KeyPairGenerator made every later lookup MISS (silently falling back to
// default algo/keysize), and a fresh allocation landing on the freed old
// address ALIASED the stale entry (wrong algorithm for an unrelated
// object). Key by identity hash + a per-hash generation instead — the same
// proven pattern as `xnio_async::xnio_obj_key_for` / native-collections'
// `widened_obj_key`. The `last_ptr` adoption handles relocation (same
// object, new address); distinct same-hash objects get distinct
// generations. Values are plain Rust data (i32/String/bool), so no GC
// scan/remap companion is needed once the keys are address-independent.
//
// VM scope: an identity hash (and a heap address) is unique only *within
// one heap*, but these tables are `static`. Rust tests create several
// independent `Vm`s in one process, so without the `vm_identity` component
// VM B's KeyPairGenerator whose hash collides with VM A's would be *adopted*
// by the `slots.len() == 1` relocation branch below and read VM A's
// algorithm / key size / provider flag. That is precisely the aliasing that
// native-collections' `widened_obj_key` hit (two VMs' collections, process
// abort); `NativeContext::vm_identity`'s doc states the rule.
struct KpgObjKeyEntry {
    last_ptr: usize,
    generation: u32,
}

/// `(vm_identity, identity_hash)` → per-VM generation slots.
type KpgHashKey = (usize, u32);

/// VM-scoped, GC-stable side-table key: `(vm_identity, packed hash+generation)`.
type KpgObjKey = (usize, usize);

fn kpg_obj_key_registry(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<KpgHashKey, Vec<KpgObjKeyEntry>>> {
    use std::sync::OnceLock;
    static R: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<KpgHashKey, Vec<KpgObjKeyEntry>>>> =
        OnceLock::new();
    R.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

#[inline]
fn pack_kpg_obj_key(hash: u32, generation: u32) -> usize {
    ((hash as usize) << 32) | (generation as usize)
}

fn kpg_obj_key_for(ctx: &dyn NativeContext, obj: ObjectRef) -> KpgObjKey {
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(obj) as u32;
    let ptr = obj.as_ptr() as usize;
    let mut reg = kpg_obj_key_registry().lock();
    let slots = reg.entry((vm, hash)).or_default();
    if let Some(slot) = slots.iter().find(|s| s.last_ptr == ptr) {
        return (vm, pack_kpg_obj_key(hash, slot.generation));
    }
    if slots.len() == 1 {
        slots[0].last_ptr = ptr;
        return (vm, pack_kpg_obj_key(hash, slots[0].generation));
    }
    let generation = slots.len() as u32;
    slots.push(KpgObjKeyEntry {
        last_ptr: ptr,
        generation,
    });
    (vm, pack_kpg_obj_key(hash, generation))
}

fn kpg_algo_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, i32>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn kpg_keysize_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, i32>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn set_kpg_algo(ctx: &dyn NativeContext, this: ObjectRef, idx: i32) {
    let key = kpg_obj_key_for(ctx, this);
    kpg_algo_table().lock().insert(key, idx);
}

fn get_kpg_algo(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let key = kpg_obj_key_for(ctx, this);
    kpg_algo_table().lock().get(&key).copied()
}

// `KeyFactory` has the same real-JDK-layout problem as KeyPairGenerator: its
// private native state sits beyond the fields declared by the JDK class. A
// raw-slot write can therefore be dropped or coerced, leaving generatePublic
// to report the placeholder algorithm "Unknown". Keep its algorithm in the
// same GC-stable identity-keyed side-table scheme.
fn kf_algo_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, i32>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, i32>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn set_kf_algo(ctx: &dyn NativeContext, this: ObjectRef, idx: i32) {
    let key = kpg_obj_key_for(ctx, this);
    kf_algo_table().lock().insert(key, idx);
}

fn get_kf_algo(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let key = kpg_obj_key_for(ctx, this);
    kf_algo_table().lock().get(&key).copied()
}

/// Preserve the caller's requested spelling for `getAlgorithm()` and diagnostic
/// errors. An algorithm index alone cannot represent unrecognised names.
fn kpg_name_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, String>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, String>>> =
        OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn set_kpg_name(ctx: &dyn NativeContext, this: ObjectRef, name: String) {
    let key = kpg_obj_key_for(ctx, this);
    kpg_name_table().lock().insert(key, name);
}

fn get_kpg_name(ctx: &dyn NativeContext, this: ObjectRef) -> Option<String> {
    let key = kpg_obj_key_for(ctx, this);
    kpg_name_table().lock().get(&key).cloned()
}

fn set_kpg_keysize(ctx: &dyn NativeContext, this: ObjectRef, bits: i32) {
    let key = kpg_obj_key_for(ctx, this);
    kpg_keysize_table().lock().insert(key, bits);
}

fn get_kpg_keysize(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let key = kpg_obj_key_for(ctx, this);
    kpg_keysize_table().lock().get(&key).copied()
}

/// Records whether a `KeyPairGenerator` was obtained via the BouncyCastle
/// provider (`getInstance(alg, "BC")`). EC keygen then produces genuine BC keys
/// (`BCECPrivate/PublicKey`) instead of SunEC `EC*KeyImpl`, so keycloak's
/// BC-specific code (`BCECDSACryptoProvider.getPublicFromPrivate`, which casts to
/// `org.bouncycastle.jce.interfaces.ECPrivateKey` and uses BC point math) works —
/// while BC keys still sign/verify through our `Signature` natives.
fn kpg_bcprov_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, bool>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<KpgObjKey, bool>>> =
        OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn set_kpg_bcprov(ctx: &dyn NativeContext, this: ObjectRef, bc: bool) {
    let key = kpg_obj_key_for(ctx, this);
    kpg_bcprov_table().lock().insert(key, bc);
}

fn get_kpg_bcprov(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let key = kpg_obj_key_for(ctx, this);
    kpg_bcprov_table()
        .lock()
        .get(&key)
        .copied()
        .unwrap_or(false)
}

/// Resolve the requested provider name from `getInstance`'s 2nd argument, which
/// is either a `String` provider name or a `java.security.Provider` instance.
fn requested_provider_name(ctx: &mut dyn NativeContext, args: &[Value]) -> String {
    let Some(Value::Object(Some(o))) = args.get(1).copied() else {
        return String::new();
    };
    let cls = ctx
        .class_name_of_id(ctx.class_id_of_object(o))
        .unwrap_or_default();
    if cls == "java/lang/String" {
        return ctx.read_string(o).unwrap_or_default();
    }
    // A `Provider` instance → getName().
    match ctx.invoke_virtual(o, "getName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(ns)))) => ctx.read_string(ns).unwrap_or_default(),
        _ => String::new(),
    }
}

fn is_bc_fips_provider(name: &str) -> bool {
    name.eq_ignore_ascii_case("BCFIPS") || name.contains("BouncyCastleFips")
}

fn is_bc_provider(name: &str) -> bool {
    name.eq_ignore_ascii_case("BC") || is_bc_fips_provider(name) || name.contains("BouncyCastle")
}

// ---------------------------------------------------------------------------
// EC-scoped real-SunEC routing (crate::route_ec_to_real, default ON)
// ---------------------------------------------------------------------------
//
// The synthetic EC keygen returns a bare `java/security/PublicKey` *interface*
// object, so keycloak's `(java.security.interfaces.ECPublicKey) pub` cast throws
// ClassCastException ("Error obtaining ECParameterSpec for P-256 curve"). When
// `crate::route_ec_to_real()` is set (default), the EC family instead DRIVES the
// real, pure-Java JDK-25 SunEC SPIs (`sun.security.ec.*`) and returns concrete
// `ECPublicKeyImpl`/`ECPrivateKeyImpl` keys + a real `KeyPair`. This runs real,
// JIT-eligible SunEC bytecode; the `AlgorithmParameters.getInstance("EC")` it
// needs is served by the provider-machinery bridge wired (also under
// `route_ec_to_real`) in `provider_chain.rs`/`cipher.rs`. RSA/AES stay synthetic
// (kill-switch `CRATONVM_SYNTHETIC_EC=1` restores the legacy synthetic EC).

/// Class name of an object (`internal/slash/form`), or empty if unknown.
fn obj_class_name(ctx: &dyn NativeContext, obj: ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default()
}

/// True if `v` is one of our synthetic bare-interface key objects whose class is
/// exactly `class_name` (`java/security/PublicKey` or `java/security/PrivateKey`).
fn is_synthetic_key_obj(ctx: &dyn NativeContext, v: &Value, class_name: &str) -> bool {
    matches!(v, Value::Object(Some(o)) if obj_class_name(ctx, *o) == class_name)
}

/// True when the named real-JDK SPI class is actually on the boot classpath.
///
/// The `route_*_to_real()` switches say we *prefer* real JDK bytecode; they do
/// not say the bytecode is *there*. In synthetic-JDK mode a name-based load of
/// e.g. `sun/security/ec/ECKeyPairGenerator` is answered with a fabricated stub
/// whose methods carry no `Code`, so "driving" the SPI yields a dead object (or
/// an opaque failure) rather than a key pair — and for EC there is no fallback
/// after the drive, so the whole `generateKeyPair` fails. Ask the class manager
/// first; when the answer is "you would get a stub", fall back to the
/// `crypto_impl` path, which is real RSA / P-256 crypto — ours rather than the
/// JDK's, but genuine key material with a working sign/verify.
fn real_spi_available(ctx: &dyn NativeContext, class_name: &str) -> bool {
    !ctx.would_fabricate_synthetic_stub(class_name)
}

/// Whether [`drive_real_ec_keypair`] would find real bytecode to drive.
///
/// Must stay in lockstep with the EC arm of `jca::signature`'s
/// `ecdsa_real_spi_class`: if keygen falls back to synthetic keys, signing has
/// to fall back too, or a synthetic `key_id`-bearing key is handed to a stub
/// SunEC SPI that cannot read it.
fn real_ec_keypair_available(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if get_kpg_bcprov(ctx, this) {
        // BouncyCastle is an application class; `would_fabricate_synthetic_stub`
        // only speaks for JDK names, so leave the BC route exactly as it was.
        return true;
    }
    real_spi_available(ctx, "sun/security/ec/ECKeyPairGenerator")
}

/// Drive the real `sun.security.ec.ECKeyPairGenerator` SPI: `new` →
/// `initialize(spec|keysize, SecureRandom)` → `generateKeyPair()`. Returns a real
/// `java.security.KeyPair` (`privateKey`@0, `publicKey`@1) of concrete
/// `ECPrivateKeyImpl`/`ECPublicKeyImpl`. Honours the requested curve: when an
/// `AlgorithmParameterSpec` (e.g. `ECGenParameterSpec("secp384r1")`) was supplied
/// via `initialize`, it is forwarded so the real SunEC code resolves the curve
/// (P-256/384/521); otherwise the stored keysize (default 256 → P-256) is used.
fn drive_real_ec_keypair(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    // BouncyCastle was explicitly requested → drive BC's EC KeyPairGenerator so
    // keycloak gets genuine `BCECPrivate/PublicKey` (its `getPublicFromPrivate`
    // casts to BC's EC key interface + uses BC point math). BC keys still
    // sign/verify through our `Signature` natives. Default = SunEC.
    let spi_class = if get_kpg_bcprov(ctx, this) {
        "org/bouncycastle/jcajce/provider/asymmetric/ec/KeyPairGeneratorSpi$EC"
    } else {
        "sun/security/ec/ECKeyPairGenerator"
    };
    drive_real_keypair_spi(ctx, this, spi_class)
}

/// Drive SunEC's curve-specific EdDSA key generators. Their public constructors
/// lock the requested curve, so no follow-up `initialize` call is required.
fn drive_real_eddsa_keypair(ctx: &mut dyn NativeContext, algo: i32) -> MethodCallResult {
    let spi_class = match algo {
        ALGO_ED25519 => "sun/security/ec/ed/EdDSAKeyPairGenerator$Ed25519",
        ALGO_ED448 => "sun/security/ec/ed/EdDSAKeyPairGenerator$Ed448",
        _ => unreachable!("EdDSA route called for non-EdDSA algorithm"),
    };
    let spi = match ctx.new_object_initialized(spi_class, "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(RuntimeError::NotImplemented {
                feature: spi_class.into(),
            }
            .into())
        }
    };
    ctx.invoke_virtual(spi, "generateKeyPair", "()Ljava/security/KeyPair;", &[])
}

/// Return the curve-specific JDK EdDSA `KeyFactorySpi` implementation for an
/// Ed25519 or Ed448 factory. Like the matching key-pair generators above,
/// these classes fix the curve in their constructor and accept
/// `EdECPublicKeySpec` directly.
fn eddsa_keyfactory_spi_class(algo: i32) -> Option<&'static str> {
    match algo {
        ALGO_ED25519 => Some("sun/security/ec/ed/EdDSAKeyFactory$Ed25519"),
        ALGO_ED448 => Some("sun/security/ec/ed/EdDSAKeyFactory$Ed448"),
        _ => None,
    }
}

/// Drive the real curve-specific JDK EdDSA `KeyFactorySpi` over the supplied
/// `EdECPublicKeySpec`, returning a concrete Ed25519/Ed448 public key. This is
/// the JWK OKP import path used by Keycloak's SD-JWT tests.
fn drive_real_eddsa_keyfactory(
    ctx: &mut dyn NativeContext,
    algo: i32,
    spec: ObjectRef,
    engine: &'static str,
    ret_desc: &'static str,
) -> MethodCallResult {
    let spi_class = eddsa_keyfactory_spi_class(algo)
        .expect("EdDSA KeyFactory route called for non-EdDSA algorithm");
    drive_keyspec_spi(ctx, spi_class, spec, engine, ret_desc)
}

/// Return the curve-specific JDK XDH `KeyFactorySpi` implementation for an
/// X25519 or X448 factory. Mirrors `eddsa_keyfactory_spi_class`: these
/// nested classes fix the curve in their (package-private, but CratonVM's
/// `new_object_initialized` constructs via direct VM-level `<init>`
/// invocation rather than `java.lang.reflect.Constructor`, so Java-level
/// accessibility never gates it — verified against real JDK 25, whose own
/// `Provider$Service.newInstance` reflection would otherwise reject this
/// exact ctor) no-argument constructor.
fn xdh_keyfactory_spi_class(algo: i32) -> Option<&'static str> {
    match algo {
        ALGO_X25519 => Some("sun/security/ec/XDHKeyFactory$X25519"),
        ALGO_X448 => Some("sun/security/ec/XDHKeyFactory$X448"),
        _ => None,
    }
}

/// Number of bytes occupied by a DER definite-length field starting at
/// `der[len_pos]`, INCLUDING the leading length-of-length byte for the
/// long form. Shared with `der_len_size`'s sibling `der_read_len` below —
/// kept separate because most callers here need "how far to skip" while
/// `is_pkcs1_rsa_private`/`rsa_pkcs1_to_pkcs8` only ever needed the former.
fn der_read_len(der: &[u8], len_pos: usize) -> Option<usize> {
    let b = *der.get(len_pos)?;
    if b < 0x80 {
        Some(b as usize)
    } else {
        let n = (b & 0x7f) as usize;
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | (*der.get(len_pos + 1 + i)? as usize);
        }
        Some(len)
    }
}

/// Position immediately after the DER TLV element whose tag byte is at
/// `pos` (i.e. `pos` + 1 tag byte + length-field bytes + content bytes).
fn der_skip_element(der: &[u8], pos: usize) -> Option<usize> {
    let len_pos = pos + 1;
    let len_size = der_len_size(der, len_pos);
    let content_len = der_read_len(der, len_pos)?;
    Some(len_pos + len_size + content_len)
}

/// 1.3.101.110 — id-X25519.
const OID_X25519: &[u8] = &[0x2b, 0x65, 0x6e];
/// 1.3.101.111 — id-X448.
const OID_X448: &[u8] = &[0x2b, 0x65, 0x6f];
/// 1.3.101.112 — id-Ed25519.
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
/// 1.3.101.113 — id-Ed448.
const OID_ED448: &[u8] = &[0x2b, 0x65, 0x71];

/// Extract the raw `AlgorithmIdentifier` OID bytes from a DER-encoded
/// `PKCS8EncodedKeySpec` (`is_private = true`: `SEQUENCE { INTEGER version,
/// AlgorithmIdentifier, OCTET STRING, ... }`) or `X509EncodedKeySpec`
/// (`is_private = false`: `SEQUENCE { AlgorithmIdentifier, BIT STRING }`),
/// without needing a full ASN.1 parser. This is how the generic `"XDH"` /
/// `"EdDSA"` `KeyFactory` names recover the true curve (X25519-vs-X448,
/// Ed25519-vs-Ed448): those algorithm strings carry no curve of their own,
/// so — exactly like real JDK's non-nested `sun.security.ec.XDHKeyFactory` /
/// `sun.security.ec.ed.EdDSAKeyFactory` — the curve has to come from the
/// spec's own embedded OID.
fn der_spec_algorithm_oid(der: &[u8], is_private: bool) -> Option<Vec<u8>> {
    if der.first() != Some(&0x30) {
        return None;
    }
    let mut pos = 1 + der_len_size(der, 1);
    if is_private {
        // INTEGER version — skip it to reach the AlgorithmIdentifier.
        if der.get(pos) != Some(&0x02) {
            return None;
        }
        pos = der_skip_element(der, pos)?;
    }
    // AlgorithmIdentifier ::= SEQUENCE { OID algorithm, ANY parameters OPTIONAL }
    if der.get(pos) != Some(&0x30) {
        return None;
    }
    let algid_content = pos + 1 + der_len_size(der, pos + 1);
    if der.get(algid_content) != Some(&0x06) {
        return None;
    }
    let oid_len_pos = algid_content + 1;
    let oid_len = der_read_len(der, oid_len_pos)?;
    let oid_start = oid_len_pos + der_len_size(der, oid_len_pos);
    der.get(oid_start..oid_start + oid_len).map(|s| s.to_vec())
}

/// Resolve a generic `ALGO_XDH_GENERIC`/`ALGO_EDDSA_GENERIC` `KeyFactory`
/// index down to the concrete curve (`ALGO_X25519`/`ALGO_X448`/
/// `ALGO_ED25519`/`ALGO_ED448`) by sniffing `spec`'s own encoded DER (field 0
/// of both `PKCS8EncodedKeySpec` and `X509EncodedKeySpec` — real JDK classes
/// with that field layout, already relied on elsewhere in this file, e.g.
/// the PKCS#1-vs-PKCS#8 RSA sniff in `kf_generate_private`). A concrete algo
/// (or an unrecognised/unparseable spec) passes through unchanged.
fn resolve_curve_algo(
    ctx: &mut dyn NativeContext,
    algo: i32,
    spec: ObjectRef,
    is_private: bool,
) -> i32 {
    if algo != ALGO_XDH_GENERIC && algo != ALGO_EDDSA_GENERIC {
        return algo;
    }
    let der = match ctx.get_field(spec, 0) {
        Value::Object(Some(arr)) => read_byte_array(ctx, arr),
        _ => return algo,
    };
    let oid = match der_spec_algorithm_oid(&der, is_private) {
        Some(o) => o,
        None => return algo,
    };
    if algo == ALGO_XDH_GENERIC {
        if oid.as_slice() == OID_X25519 {
            return ALGO_X25519;
        }
        if oid.as_slice() == OID_X448 {
            return ALGO_X448;
        }
    } else if algo == ALGO_EDDSA_GENERIC {
        if oid.as_slice() == OID_ED25519 {
            return ALGO_ED25519;
        }
        if oid.as_slice() == OID_ED448 {
            return ALGO_ED448;
        }
    }
    algo
}

/// Drive the real curve-specific JDK EdDSA/XDH `KeyFactorySpi`
/// (`engineGeneratePublic`/`engineGeneratePrivate`) for `algo`, which may
/// already be a concrete curve or one of the generic sentinels
/// (`ALGO_XDH_GENERIC`/`ALGO_EDDSA_GENERIC` — resolved against `spec`'s own
/// embedded OID via `resolve_curve_algo` first). Returns `None` when `algo`
/// isn't an EdDSA/XDH algorithm at all, so callers fall through to their
/// other branches unchanged.
fn drive_eddsa_or_xdh_keyfactory(
    ctx: &mut dyn NativeContext,
    algo: i32,
    spec: ObjectRef,
    engine: &'static str,
    ret_desc: &'static str,
) -> Option<MethodCallResult> {
    if !matches!(
        algo,
        ALGO_ED25519 | ALGO_ED448 | ALGO_X25519 | ALGO_X448 | ALGO_XDH_GENERIC | ALGO_EDDSA_GENERIC
    ) {
        return None;
    }
    let is_private = engine == "engineGeneratePrivate";
    let resolved = resolve_curve_algo(ctx, algo, spec, is_private);
    let spi =
        eddsa_keyfactory_spi_class(resolved).or_else(|| xdh_keyfactory_spi_class(resolved))?;
    Some(drive_keyspec_spi(ctx, spi, spec, engine, ret_desc))
}

/// Drive a real JDK `KeyPairGenerator`, honouring the stored key size and an
/// optional `AlgorithmParameterSpec`, then returning a real `KeyPair`.
///
/// SunEC needs direct allocation to avoid its no-argument constructor's
/// default-curve initialization; other provider generators use their public
/// no-argument constructor.
fn drive_real_keypair_spi(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    spi_class: &'static str,
) -> MethodCallResult {
    // Read the requested keysize + spec (curve) BEFORE any allocation.
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let keysize = get_kpg_keysize(ctx, this)
        .filter(|n| *n > 0)
        .or_else(|| match ctx.get_field(this, base + KPG_OFF_KEYSIZE) {
            Value::Int(n) if n > 0 => Some(n),
            _ => None,
        })
        .unwrap_or(256);
    let spec0 = match ctx.get_field(this, base + KPG_OFF_SPEC) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    // Pin the spec (if any) FIRST so it survives the SPI allocation below.
    let spec_pin = spec0.map(|s| ctx.pin_native_root(s));
    let spi = if spi_class == "sun/security/ec/ECKeyPairGenerator" {
        // SunEC's no-arg constructor initializes the provider default key size
        // before callers can pass ECGenParameterSpec. JDK 25's default is 384,
        // which fails under CratonVM's partial SunEC provider map; allocate the
        // SPI directly and invoke the requested initialize(...) below.
        ctx.ensure_class_initialized(spi_class)?;
        match ctx.allocate_instance(spi_class) {
            Some(o) => o,
            None => {
                if let Some(p) = spec_pin {
                    ctx.unpin_native_roots(p);
                }
                return Err(RuntimeError::NotImplemented {
                    feature: spi_class.into(),
                }
                .into());
            }
        }
    } else {
        match ctx.new_object_initialized(spi_class, "()V", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => {
                if let Some(p) = spec_pin {
                    ctx.unpin_native_roots(p);
                }
                other?;
                return Err(RuntimeError::NotImplemented {
                    feature: spi_class.into(),
                }
                .into());
            }
        }
    };
    let spi_pin = ctx.pin_native_root(spi);
    let unpin_base = spec_pin.unwrap_or(spi_pin);
    let result = (|| {
        let rnd = match ctx.new_object_initialized("java/security/SecureRandom", "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: "java.security.SecureRandom".into(),
                }
                .into())
            }
        };
        let spi = ctx.read_native_pin(spi_pin, spi);
        match (spec0, spec_pin) {
            (Some(s0), Some(sp)) => {
                let s = ctx.read_native_pin(sp, s0);
                ctx.invoke_virtual(
                    spi,
                    "initialize",
                    "(Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
                    &[Value::Object(Some(s)), Value::Object(Some(rnd))],
                )?;
            }
            _ => {
                ctx.invoke_virtual(
                    spi,
                    "initialize",
                    "(ILjava/security/SecureRandom;)V",
                    &[Value::Int(keysize), Value::Object(Some(rnd))],
                )?;
            }
        }
        let spi = ctx.read_native_pin(spi_pin, spi);
        ctx.invoke_virtual(spi, "generateKeyPair", "()Ljava/security/KeyPair;", &[])
    })();
    ctx.unpin_native_roots(unpin_base);
    result
}

/// Drive the real `sun.security.ec.ECKeyFactory` SPI's `engineGeneratePublic` /
/// `engineGeneratePrivate` over the supplied `EC{Public,Private}KeySpec`, yielding
/// a concrete `sun.security.ec.EC{Public,Private}KeyImpl`.
fn drive_real_ec_keyfactory(
    ctx: &mut dyn NativeContext,
    spec: ObjectRef,
    engine: &'static str,
    ret_desc: &'static str,
) -> MethodCallResult {
    drive_keyspec_spi(ctx, "sun/security/ec/ECKeyFactory", spec, engine, ret_desc)
}

/// Drive the real `sun.security.provider.DSAKeyFactory` SPI. CratonVM has no
/// synthetic DSA key material to fall back to (unlike RSA/EC) — see the
/// `ALGO_DSA` doc comment for the root-cause story.
fn drive_real_dsa_keyfactory(
    ctx: &mut dyn NativeContext,
    spec: ObjectRef,
    engine: &'static str,
    ret_desc: &'static str,
) -> MethodCallResult {
    drive_keyspec_spi(
        ctx,
        "sun/security/provider/DSAKeyFactory",
        spec,
        engine,
        ret_desc,
    )
}

/// Drive the real JDK `sun.security.provider.DSAKeyPairGenerator$Current`.
///
/// DSA has no compatible synthetic key representation: consumers expect the
/// concrete `DSAPublicKey` / `DSAPrivateKey` objects, including their domain
/// parameters.  Use the same real-SPI path as EC so default generation and
/// callers that explicitly initialize a key size both return genuine JDK keys.
/// `Current` is the JDK 25 provider implementation selected for ordinary
/// `KeyPairGenerator.getInstance("DSA")` calls.
fn drive_real_dsa_keypair(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    drive_real_keypair_spi(
        ctx,
        this,
        "sun/security/provider/DSAKeyPairGenerator$Current",
    )
}

/// Construct the real `KeyFactorySpi` named by `spi_class` and invoke its
/// `engine{Generate,GetKeySpec}` method over `spec`. The KeySpec is pinned across
/// the SPI allocation. Used for SunEC (the default EC path) and for BouncyCastle's
/// `ec.KeyFactorySpi$EC` fallback that natively accepts BC-specific specs
/// (`org.bouncycastle.jce.spec.EC{Public,Private}KeySpec`).
fn drive_keyspec_spi(
    ctx: &mut dyn NativeContext,
    spi_class: &'static str,
    spec: ObjectRef,
    engine: &'static str,
    ret_desc: &'static str,
) -> MethodCallResult {
    let pin = ctx.pin_native_root(spec);
    let result = (|| {
        let kf = match ctx.new_object_initialized(spi_class, "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: spi_class.into(),
                }
                .into())
            }
        };
        let spec = ctx.read_native_pin(pin, spec);
        let desc = format!("(Ljava/security/spec/KeySpec;){ret_desc}");
        ctx.invoke_virtual(kf, engine, &desc, &[Value::Object(Some(spec))])
    })();
    ctx.unpin_native_roots(pin);
    result
}

/// Drive the real SunRsaSign `RSAKeyFactory$Legacy` SPI's `engineGenerate*`
/// over the supplied RSA key spec (RSAPrivateCrtKeySpec / RSAPrivateKeySpec /
/// RSAPublicKeySpec / PKCS8EncodedKeySpec / X509EncodedKeySpec), yielding a
/// concrete `sun.security.rsa.RSAPrivate{Crt}KeyImpl` / `RSAPublicKeyImpl` that
/// holds the spec's real components. Mirrors `drive_real_ec_keyfactory`. The
/// caller falls back to `InvalidKeySpecException` if this returns an error, so
/// behaviour is no worse than the synthetic fail-closed path.
fn drive_real_rsa_keyfactory(
    ctx: &mut dyn NativeContext,
    spec: ObjectRef,
    engine: &'static str,
    ret_desc: &'static str,
) -> MethodCallResult {
    let pin = ctx.pin_native_root(spec);
    let result = (|| {
        let kf = match ctx.new_object_initialized(
            "sun/security/rsa/RSAKeyFactory$Legacy",
            "()V",
            &[],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: "sun.security.rsa.RSAKeyFactory$Legacy".into(),
                }
                .into())
            }
        };
        let spec = ctx.read_native_pin(pin, spec);
        let desc = format!("(Ljava/security/spec/KeySpec;){ret_desc}");
        ctx.invoke_virtual(kf, engine, &desc, &[Value::Object(Some(spec))])
    })();
    ctx.unpin_native_roots(pin);
    result
}

/// Drive the real SunRsaSign `RSAKeyFactory$PSS` SPI — like
/// `drive_real_rsa_keyfactory`, but for `KeyFactory.getInstance("RSASSA-PSS")`
/// (`ALGO_RSASSA_PSS`), which is a STRICTER, distinct real SPI from the
/// permissive `$Legacy` that "RSA" drives: `$PSS` requires the PKCS#8/X.509
/// `AlgorithmIdentifier` OID to be exactly `id-RSASSA-PSS`, whereas `$Legacy`
/// REJECTS that OID outright (`InvalidKeyException: Expected a RSA key, but
/// got RSASSA-PSS`, verified against real JDK 25). See `ALGO_RSASSA_PSS`'s
/// doc comment for why `PemPrivateKeyParser`'s "RSA"-then-"RSASSA-PSS"
/// fallback loop needs this distinct route to succeed on its second try.
fn drive_real_rsa_pss_keyfactory(
    ctx: &mut dyn NativeContext,
    spec: ObjectRef,
    engine: &'static str,
    ret_desc: &'static str,
) -> MethodCallResult {
    let pin = ctx.pin_native_root(spec);
    let result = (|| {
        let kf =
            match ctx.new_object_initialized("sun/security/rsa/RSAKeyFactory$PSS", "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: "sun.security.rsa.RSAKeyFactory$PSS".into(),
                }
                .into())
            }
        };
        let spec = ctx.read_native_pin(pin, spec);
        let desc = format!("(Ljava/security/spec/KeySpec;){ret_desc}");
        ctx.invoke_virtual(kf, engine, &desc, &[Value::Object(Some(spec))])
    })();
    ctx.unpin_native_roots(pin);
    result
}

/// Build a positive `java.math.BigInteger` from an unsigned big-endian
/// magnitude. Uses `BigInteger(int signum, byte[] magnitude)` so a leading
/// high bit is never misread as a negative two's-complement value.
fn build_positive_biginteger(
    ctx: &mut dyn NativeContext,
    magnitude: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = alloc_byte_array(ctx, magnitude);
    match ctx.new_object_initialized(
        "java/math/BigInteger",
        "(I[B)V",
        &[Value::Int(1), Value::Object(Some(arr))],
    )? {
        Some(Value::Object(Some(o))) => Ok(o),
        _ => Err(RuntimeError::NotImplemented {
            feature: "java.math.BigInteger(int,byte[])".into(),
        }
        .into()),
    }
}

/// Invoke `obj.method()` → `BigInteger`, then `BigInteger.toByteArray()`, into a
/// raw two's-complement big-endian magnitude. `None` if either virtual call
/// fails. The intermediate `BigInteger` is pinned across `toByteArray()`.
fn call_biginteger_to_bytes(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    method: &str,
) -> Option<Vec<u8>> {
    let bi = match ctx.invoke_virtual(obj, method, "()Ljava/math/BigInteger;", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
    };
    let pin = ctx.pin_native_root(bi);
    let bi = ctx.read_native_pin(pin, bi);
    let arr = ctx.invoke_virtual(bi, "toByteArray", "()[B", &[]);
    ctx.unpin_native_roots(pin);
    match arr {
        Ok(Some(Value::Object(Some(a)))) => Some(read_byte_array(ctx, a)),
        _ => None,
    }
}

/// Read `(modulus, publicExponent)` magnitudes from a `java.security.spec.
/// RSAPublicKeySpec`. Used by the synthetic (`CRATONVM_SYNTHETIC_RSA=1`)
/// `generatePublic` path — the counterpart to the real-KeyFactory drive used
/// when `route_rsa_to_real()` is on — so keycloak's JWK→key conversion
/// (`generatePublic(new RSAPublicKeySpec(n, e))`) works in both modes.
fn rsa_pubspec_components(
    ctx: &mut dyn NativeContext,
    spec: ObjectRef,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let pin = ctx.pin_native_root(spec);
    let spec_r = ctx.read_native_pin(pin, spec);
    let n = call_biginteger_to_bytes(ctx, spec_r, "getModulus");
    let spec_r = ctx.read_native_pin(pin, spec);
    let e = call_biginteger_to_bytes(ctx, spec_r, "getPublicExponent");
    ctx.unpin_native_roots(pin);
    match (n, e) {
        (Some(n), Some(e)) if !n.is_empty() && !e.is_empty() => Some((n, e)),
        _ => None,
    }
}

/// Materialise a real `sun.security.rsa.RSAPublic/PrivateKeyImpl` from raw RSA
/// components via `RSA{Public,Private}KeySpec` → real `RSAKeyFactory$Legacy`,
/// and register the GC-stable `identityHashCode(key) -> key_id` bridge so the
/// `Signature` natives keep using the fast `crypto_impl` sign/verify even though
/// the returned key carries no synthetic `key_id` slot.
///
/// `second` is the public exponent `e` (public key) or the private exponent `d`
/// (private key). `crypto_impl`'s RSA holds exactly `{n, e, d}` (no CRT primes),
/// and the `RSA{Public,Private}KeySpec` path needs only modulus + one exponent —
/// so this re-imports our *own* already-generated material, NOT a slow keygen.
fn real_rsa_key_from_components(
    ctx: &mut dyn NativeContext,
    n_bytes: &[u8],
    second: &[u8],
    key_id: u64,
    is_public: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let (spec_class, engine, ret_desc) = if is_public {
        (
            "java/security/spec/RSAPublicKeySpec",
            "engineGeneratePublic",
            "Ljava/security/PublicKey;",
        )
    } else {
        (
            "java/security/spec/RSAPrivateKeySpec",
            "engineGeneratePrivate",
            "Ljava/security/PrivateKey;",
        )
    };
    // Build the two BigIntegers, pinning the modulus across the second
    // allocation (the moving collector may relocate it).
    let n_bi = build_positive_biginteger(ctx, n_bytes)?;
    let p0 = ctx.pin_native_root(n_bi);
    let built = (|| {
        let s_bi = build_positive_biginteger(ctx, second)?;
        let p1 = ctx.pin_native_root(s_bi);
        let n_bi = ctx.read_native_pin(p0, n_bi);
        let s_bi = ctx.read_native_pin(p1, s_bi);
        let spec = match ctx.new_object_initialized(
            spec_class,
            "(Ljava/math/BigInteger;Ljava/math/BigInteger;)V",
            &[Value::Object(Some(n_bi)), Value::Object(Some(s_bi))],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: spec_class.into(),
                }
                .into())
            }
        };
        drive_real_rsa_keyfactory(ctx, spec, engine, ret_desc)
    })();
    ctx.unpin_native_roots(p0);
    let key = match built? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(RuntimeError::NotImplemented {
                feature: "RSAKeyFactory$Legacy produced no key".into(),
            }
            .into())
        }
    };
    // VM-scoped key: an identity hash is unique only within one heap, and
    // `RSA_REALKEY_MAP` is a process-global static (see its doc comment).
    crypto_impl::rsa_realkey_map_set(ctx.vm_identity(), ctx.identity_hash_code(key), key_id);
    Ok(key)
}

/// Build a GENUINE CRT `RSAPrivateCrtKeyImpl` from the full component set via
/// `RSAPrivateCrtKeySpec` + the real `RSAKeyFactory$Legacy` SPI.
///
/// The 2-arg `real_rsa_key_from_components(.., is_public=false)` path builds an
/// `RSAPrivateKeySpec(n, d)` → `sun.security.rsa.RSAPrivateKeyImpl` (non-CRT),
/// whose `getEncoded()` is an incomplete 572-byte PKCS#8 (only n and d; e and
/// all CRT params encoded as INTEGER 0) that rustls rejects
/// (`failed to parse private key as RSA`) — the root cause behind
/// fixed-suite-bugs/http-server-sslengine-identity-singleton-clobber-FIXED.md. A
/// freshly generated key HAS its CRT parameters, so build the CRT spec and get
/// a real `RSAPrivateCrtKeyImpl` with a complete `getEncoded()`.
///
/// `comps` is `[n, e, d, p, q, dp, dq, qinv]`, each an unsigned big-endian
/// magnitude. Every intermediate `BigInteger` is pinned across the subsequent
/// allocations (the moving collector can relocate them); `new_object_initialized`
/// then GC-roots its own init args.
#[allow(clippy::too_many_arguments)]
fn real_rsa_crt_private_key(
    ctx: &mut dyn NativeContext,
    comps: [&[u8]; 8],
    key_id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    // Build all eight BigIntegers, keeping every previously-built one pinned
    // across each new allocation. `pin_native_root` returns the slot index;
    // the first pin's index is the release watermark.
    let mut objs: [Option<ObjectRef>; 8] = [None; 8];
    let mut handles: [usize; 8] = [0; 8];
    let mut base: Option<usize> = None;
    for (i, bytes) in comps.iter().enumerate() {
        let bi = build_positive_biginteger(ctx, bytes)?;
        let h = ctx.pin_native_root(bi);
        if base.is_none() {
            base = Some(h);
        }
        handles[i] = h;
        objs[i] = Some(bi);
    }
    // Re-read each forwarded ref (a later allocation may have relocated it).
    let mut args: Vec<Value> = Vec::with_capacity(8);
    for i in 0..8 {
        let cur = objs[i].expect("bigint built above");
        let fwd = ctx.read_native_pin(handles[i], cur);
        args.push(Value::Object(Some(fwd)));
    }
    let built = (|| {
        let spec = match ctx.new_object_initialized(
            "java/security/spec/RSAPrivateCrtKeySpec",
            "(Ljava/math/BigInteger;Ljava/math/BigInteger;Ljava/math/BigInteger;\
             Ljava/math/BigInteger;Ljava/math/BigInteger;Ljava/math/BigInteger;\
             Ljava/math/BigInteger;Ljava/math/BigInteger;)V",
            &args,
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NotImplemented {
                    feature: "java.security.spec.RSAPrivateCrtKeySpec".into(),
                }
                .into())
            }
        };
        drive_real_rsa_keyfactory(
            ctx,
            spec,
            "engineGeneratePrivate",
            "Ljava/security/PrivateKey;",
        )
    })();
    if let Some(b) = base {
        ctx.unpin_native_roots(b);
    }
    let key = match built? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(RuntimeError::NotImplemented {
                feature: "RSAKeyFactory$Legacy produced no CRT key".into(),
            }
            .into())
        }
    };
    // VM-scoped key -- see `RSA_REALKEY_MAP`'s doc comment.
    crypto_impl::rsa_realkey_map_set(ctx.vm_identity(), ctx.identity_hash_code(key), key_id);
    Ok(key)
}

/// Build both real RSA key objects from raw components and assemble them into a
/// GENUINE `java.security.KeyPair` via its real constructor — so `getPublic()` /
/// `getPrivate()` observe the correct `publicKey` / `privateKey` fields (the
/// real layout is `privateKey@0, publicKey@1`, which the `keypair_get_*`
/// accessors handle for real keys). Pins the public key across the private-key
/// construction (which allocates and may relocate the heap). The crypto material
/// is already stored under `key_id`, so sign/verify stay on the fast Rust path.
///
/// `crt_priv`, when `Some([p, q, dp, dq, qinv])`, builds the private key as a
/// full CRT `RSAPrivateCrtKeyImpl` (complete `getEncoded()`); `None` falls back
/// to the legacy 2-arg `(n, d)` non-CRT key.
#[allow(clippy::too_many_arguments)]
fn real_rsa_keypair(
    ctx: &mut dyn NativeContext,
    n_bytes: &[u8],
    e_bytes: &[u8],
    d_bytes: &[u8],
    crt_priv: Option<[&[u8]; 5]>,
    key_id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    let pub_obj = real_rsa_key_from_components(ctx, n_bytes, e_bytes, key_id, true)?;
    let pin = ctx.pin_native_root(pub_obj);
    let assembled = (|| {
        let priv_obj = match crt_priv {
            Some([p, q, dp, dq, qinv]) => real_rsa_crt_private_key(
                ctx,
                [n_bytes, e_bytes, d_bytes, p, q, dp, dq, qinv],
                key_id,
            )?,
            None => real_rsa_key_from_components(ctx, n_bytes, d_bytes, key_id, false)?,
        };
        // `new_object_initialized` is GC-safe for its init args (VM override), so
        // `priv_obj` needs no separate pin; refresh `pub_obj` post-relocation.
        let pub_obj = ctx.read_native_pin(pin, pub_obj);
        match ctx.new_object_initialized(
            "java/security/KeyPair",
            "(Ljava/security/PublicKey;Ljava/security/PrivateKey;)V",
            &[Value::Object(Some(pub_obj)), Value::Object(Some(priv_obj))],
        )? {
            Some(Value::Object(Some(o))) => Ok(o),
            _ => Err(RuntimeError::NotImplemented {
                feature: "java.security.KeyPair(PublicKey,PrivateKey)".into(),
            }
            .into()),
        }
    })();
    ctx.unpin_native_roots(pin);
    assembled
}

/// Reconstruct a real EC/RSA `PublicKey` from a X.509 `SubjectPublicKeyInfo` DER
/// blob, routing through the real KeyFactory SPIs (SunEC for EC under
/// `route_ec_to_real`, real RSA under `route_rsa_to_real`). Returns `Object(None)`
/// for any other algorithm (the caller then mirrors BouncyCastle's own
/// "no converter -> null" behaviour). The RSA key is bridged for fast verify via
/// the identity map; EC verifies through the real key object directly.
fn real_public_key_from_x509_der(ctx: &mut dyn NativeContext, der: &[u8]) -> MethodCallResult {
    // OID TLVs as they appear inside the AlgorithmIdentifier SEQUENCE.
    const EC_OID: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]; // 1.2.840.10045.2.1
    const RSA_OID: &[u8] = &[
        0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
    ]; // 1.2.840.113549.1.1.1
    let has = |needle: &[u8]| der.windows(needle.len()).any(|w| w == needle);
    if has(EC_OID) && crate::route_ec_to_real() {
        let arr = alloc_byte_array(ctx, der);
        let spec = match ctx.new_object_initialized(
            "java/security/spec/X509EncodedKeySpec",
            "([B)V",
            &[Value::Object(Some(arr))],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        return drive_real_ec_keyfactory(
            ctx,
            spec,
            "engineGeneratePublic",
            "Ljava/security/PublicKey;",
        );
    }
    if has(RSA_OID) && crate::route_rsa_to_real() {
        if let Some(pk) = crypto_impl::parse_rsa_public_key(der) {
            let n = pk.n.to_bytes_be();
            let e = pk.e.to_bytes_be();
            let key_id = crypto_impl::rsa_key_next_id();
            crypto_impl::rsa_key_store(
                key_id,
                crypto_impl::RsaKeyPairData {
                    public_key: pk,
                    private_key: crypto_impl::RsaPrivateKey {
                        n: crypto_impl::BigUint::from_bytes_be(&[1]),
                        d: crypto_impl::BigUint::from_bytes_be(&[1]),
                        e: crypto_impl::BigUint::from_bytes_be(&[1]),
                        p: None,
                        q: None,
                        dp: None,
                        dq: None,
                        qinv: None,
                    },
                },
            );
            if let Ok(key) = real_rsa_key_from_components(ctx, &n, &e, key_id, true) {
                return Ok(Some(Value::Object(Some(key))));
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

/// `org.bouncycastle.jce.provider.BouncyCastleProvider.getPublicKey(SubjectPublicKeyInfo)`
/// (static). BC's EC key-info-converter is never registered because CratonVM
/// no-ops `EC$Mappings.configure` (to dodge the ~5-min `EC.<clinit>` curve-table
/// walk), so BC's own `getPublicKey` returns null for EC certs — which makes
/// `X509CertificateObject.getPublicKey()` null and blows up keycloak cert
/// generation (`caCert.getPublicKey().getAlgorithm()` NPE, hits EC *and* RSA
/// flows). Reconstruct EC/RSA keys from the SPKI's X.509 DER via the real
/// KeyFactories instead. (RSA already worked via BC, but routing it here too is
/// equivalent — a real `RSAPublicKeyImpl`, matching HotSpot's no-provider path.)
fn bc_provider_get_public_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let spki = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pin = ctx.pin_native_root(spki);
    let out = (|| {
        let spki = ctx.read_native_pin(pin, spki);
        let der = match ctx.invoke_virtual(spki, "getEncoded", "()[B", &[])? {
            Some(Value::Object(Some(arr))) => read_byte_array(ctx, arr),
            _ => return Ok(Some(Value::Object(None))),
        };
        real_public_key_from_x509_der(ctx, &der)
    })();
    ctx.unpin_native_roots(pin);
    out
}

/// Register a real imported RSA public key's verify material via its own X.509
/// encoding, so `Signature.verify` stays on the fast crypto_impl path (the real
/// key carries no synthetic `key_id` slot). Used by the `generatePublic` import
/// path which may receive an `RSAPublicKeySpec` (not a DER we can pre-parse).
fn register_rsa_pub_verify_material(ctx: &mut dyn NativeContext, key: ObjectRef) {
    let pin = ctx.pin_native_root(key);
    let key = ctx.read_native_pin(pin, key);
    let enc = ctx.invoke_virtual(key, "getEncoded", "()[B", &[]);
    let key = ctx.read_native_pin(pin, key);
    ctx.unpin_native_roots(pin);
    if let Ok(Some(Value::Object(Some(arr)))) = enc {
        let der = read_byte_array(ctx, arr);
        if let Some(pk) = crypto_impl::parse_rsa_public_key(&der) {
            let key_id = crypto_impl::rsa_key_next_id();
            crypto_impl::rsa_key_store(
                key_id,
                crypto_impl::RsaKeyPairData {
                    public_key: pk,
                    private_key: crypto_impl::RsaPrivateKey {
                        n: crypto_impl::BigUint::from_bytes_be(&[1]),
                        d: crypto_impl::BigUint::from_bytes_be(&[1]),
                        e: crypto_impl::BigUint::from_bytes_be(&[1]),
                        p: None,
                        q: None,
                        dp: None,
                        dq: None,
                        qinv: None,
                    },
                },
            );
            // VM-scoped key -- see `RSA_REALKEY_MAP`'s doc comment.
            crypto_impl::rsa_realkey_map_set(
                ctx.vm_identity(),
                ctx.identity_hash_code(key),
                key_id,
            );
        }
    }
}

// Synthetic-slot offsets relative to `synthetic_base_offset(...)`.
const KPG_OFF_ALGO: usize = 0;
const KPG_OFF_KEYSIZE: usize = 1;
const KPG_OFF_STATE: usize = 2;
// Slot 3 holds the `AlgorithmParameterSpec` passed to `initialize(spec[,random])`
// (e.g. ECGenParameterSpec) so the real EC keygen drive can honour the requested
// curve (P-256/384/521) instead of assuming P-256. GC-scanned synthetic slot.
const KPG_OFF_SPEC: usize = 3;
const KPG_PRIVATE_SLOTS: usize = 4;

const KF_OFF_ALGO: usize = 0;
const KF_PRIVATE_SLOTS: usize = 1;

// `PublicKey` / `PrivateKey` are interfaces in JDK 25 (no instance fields),
// so we can keep using the legacy fixed slot layout here.
const KEY_FIELD_ALGO: usize = 0;
const KEY_FIELD_BITS: usize = 1;
const KEY_FIELD_ENCLEN: usize = 2;
const KEY_FIELD_KEYID: usize = 3;
const KEY_FIELD_DER: usize = 4;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Invoke a `()Ljava/math/BigInteger;` accessor on `key` and return the
/// magnitude as unsigned big-endian bytes (stripping the two's-complement sign
/// byte). Empty on any failure (e.g. a non-CRT key has no `getPublicExponent`).
fn read_biginteger_magnitude(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
    accessor: &str,
) -> Vec<u8> {
    let bi = match ctx.invoke_virtual(key, accessor, "()Ljava/math/BigInteger;", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Vec::new(),
    };
    let arr = match ctx.invoke_virtual(bi, "toByteArray", "()[B", &[]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => return Vec::new(),
    };
    let mut bytes = read_byte_array(ctx, arr);
    // BigInteger.toByteArray() prepends a 0x00 sign byte for positive values
    // whose high bit is set — strip leading zeros so the magnitude is unsigned.
    while bytes.len() > 1 && bytes[0] == 0 {
        bytes.remove(0);
    }
    bytes
}

/// Bridge a real *imported* RSA private key (`RSAPrivate{Crt}KeyImpl`) to a
/// `crypto_impl` key_id by reading its modulus/exponents, so signing through
/// CratonVM's `Signature` natives uses the fast Rust path. Without this an
/// imported private key carries no synthetic `key_id` and `rsa_sign(0)` yields
/// a garbage signature — keycloak's `KeyPairVerifier` (sign "content" then
/// verify) then reports "Keys don't match".
fn register_rsa_priv_sign_material(ctx: &mut dyn NativeContext, key: ObjectRef) {
    let pin = ctx.pin_native_root(key);
    let k = ctx.read_native_pin(pin, key);
    let n = read_biginteger_magnitude(ctx, k, "getModulus");
    let k = ctx.read_native_pin(pin, key);
    let d = read_biginteger_magnitude(ctx, k, "getPrivateExponent");
    let k = ctx.read_native_pin(pin, key);
    let e = read_biginteger_magnitude(ctx, k, "getPublicExponent");
    let k = ctx.read_native_pin(pin, key);
    let ihash = ctx.identity_hash_code(k);
    ctx.unpin_native_roots(pin);
    if n.is_empty() || d.is_empty() {
        return;
    }
    // Non-CRT private keys expose no public exponent — default to F4 (65537).
    let e = if e.is_empty() {
        vec![0x01, 0x00, 0x01]
    } else {
        e
    };
    let key_id = crypto_impl::rsa_key_next_id();
    crypto_impl::rsa_key_store(
        key_id,
        crypto_impl::RsaKeyPairData {
            public_key: crypto_impl::RsaPublicKey {
                n: crypto_impl::BigUint::from_bytes_be(&n),
                e: crypto_impl::BigUint::from_bytes_be(&e),
            },
            private_key: crypto_impl::RsaPrivateKey {
                n: crypto_impl::BigUint::from_bytes_be(&n),
                d: crypto_impl::BigUint::from_bytes_be(&d),
                e: crypto_impl::BigUint::from_bytes_be(&e),
                p: None,
                q: None,
                dp: None,
                dq: None,
                qinv: None,
            },
        },
    );
    // VM-scoped key -- see `RSA_REALKEY_MAP`'s doc comment. `ihash` was
    // captured above while `key` was still pinned; `vm_identity()` is a
    // property of the context, not the object, so it is safe to read here.
    crypto_impl::rsa_realkey_map_set(ctx.vm_identity(), ihash, key_id);
}

fn read_string(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> String {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    }
}

fn this_arg(args: &[Value]) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("null this".into()),
        }
        .into()),
    }
}

fn algo_idx(name: &str) -> i32 {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        "ML-KEM-512" => 0,
        "ML-KEM-768" => 1,
        "ML-KEM-1024" => 2,
        "ML-DSA-44" => 3,
        "ML-DSA-65" => 4,
        "ML-DSA-87" => 5,
        "RSA" => ALGO_RSA,
        // RSASSA-PSS uses standard RSA key material; the PSS choice belongs to
        // Signature, not KeyPairGenerator.
        "RSASSA-PSS" => ALGO_RSA,
        "EC" | "ECDSA" => ALGO_EC,
        "ED25519" | "EDDSA" => ALGO_ED25519,
        "ED448" => ALGO_ED448,
        "X25519" => ALGO_X25519,
        "DSA" | "DSS" => ALGO_DSA,
        _ => -1,
    }
}

fn algo_name(idx: i32) -> &'static str {
    match idx {
        0 => "ML-KEM-512",
        1 => "ML-KEM-768",
        2 => "ML-KEM-1024",
        3 => "ML-DSA-44",
        4 => "ML-DSA-65",
        5 => "ML-DSA-87",
        ALGO_RSA => "RSA",
        ALGO_EC => "EC",
        ALGO_ED25519 => "Ed25519",
        ALGO_ED448 => "Ed448",
        ALGO_X25519 => "X25519",
        ALGO_X448 => "X448",
        ALGO_XDH_GENERIC => "XDH",
        ALGO_EDDSA_GENERIC => "EdDSA",
        ALGO_RSASSA_PSS => "RSASSA-PSS",
        ALGO_DSA => "DSA",
        _ => "Unknown",
    }
}

/// `KeyFactory`-specific algorithm resolution. Delegates to the shared
/// `algo_idx` (also used by `KeyPairGenerator`) for every name whose
/// `KeyFactory` behaviour matches its `KeyPairGenerator` behaviour, except
/// for the handful whose real-JDK `KeyFactory` SPI genuinely differs — see
/// the `ALGO_XDH_GENERIC` / `ALGO_EDDSA_GENERIC` / `ALGO_RSASSA_PSS` doc
/// comments for why each of these needs its own index rather than collapsing
/// onto the `KeyPairGenerator`-facing mapping.
fn kf_algo_idx(name: &str) -> i32 {
    match name.to_ascii_uppercase().as_str() {
        "RSASSA-PSS" => ALGO_RSASSA_PSS,
        "XDH" => ALGO_XDH_GENERIC,
        "EDDSA" => ALGO_EDDSA_GENERIC,
        "X25519" => ALGO_X25519,
        "X448" => ALGO_X448,
        _ => algo_idx(name),
    }
}

fn alloc_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    arr
}

fn read_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push(b as u8);
        }
    }
    out
}

/// Build a 4-field public key with stored DER + key_id.
fn alloc_public_key(
    ctx: &mut dyn NativeContext,
    algo: i32,
    bits: i32,
    der: &[u8],
    key_id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/security/PublicKey", 5)?;
    ctx.set_field(obj, KEY_FIELD_ALGO, Value::Int(algo));
    ctx.set_field(obj, KEY_FIELD_BITS, Value::Int(bits));
    ctx.set_field(obj, KEY_FIELD_ENCLEN, Value::Int(der.len() as i32));
    ctx.set_field(obj, KEY_FIELD_KEYID, Value::Long(key_id as i64));
    let arr = alloc_byte_array(ctx, der);
    ctx.set_field(obj, KEY_FIELD_DER, Value::Object(Some(arr)));
    Ok(obj)
}

fn alloc_private_key(
    ctx: &mut dyn NativeContext,
    algo: i32,
    bits: i32,
    der: &[u8],
    key_id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/security/PrivateKey", 5)?;
    ctx.set_field(obj, KEY_FIELD_ALGO, Value::Int(algo));
    ctx.set_field(obj, KEY_FIELD_BITS, Value::Int(bits));
    ctx.set_field(obj, KEY_FIELD_ENCLEN, Value::Int(der.len() as i32));
    ctx.set_field(obj, KEY_FIELD_KEYID, Value::Long(key_id as i64));
    let arr = alloc_byte_array(ctx, der);
    ctx.set_field(obj, KEY_FIELD_DER, Value::Object(Some(arr)));
    Ok(obj)
}

fn alloc_keypair(ctx: &mut dyn NativeContext, pubk: ObjectRef, privk: ObjectRef) -> Result<ObjectRef, MethodCallFailed> {
    let kp = try_alloc_concurrent_synthetic(ctx, "java/security/KeyPair", 2)?;
    ctx.set_field(kp, 0, Value::Object(Some(pubk)));
    ctx.set_field(kp, 1, Value::Object(Some(privk)));
    Ok(kp)
}

/// Construct and throw a real JCA exception of `class_name` (internal form,
/// e.g. `java/security/NoSuchAlgorithmException` or
/// `java/security/spec/InvalidKeySpecException`) carrying `msg`. These are all
/// `GeneralSecurityException` subclasses, so Java callers catch them exactly as
/// under HotSpot.
///
/// No-synthetic-stubs policy: the fallbacks these replace minted keys with
/// empty DER / `key_id == 0`, presenting failed keygen or key import as
/// success — any app that then signed/encrypted/verified with one got garbage
/// or a false success. We fail loudly instead. If the real exception class
/// can't be constructed we still raise a catchable `SecurityException` rather
/// than returning an empty/unusable key.
fn throw_jca(ctx: &mut dyn NativeContext, class_name: &str, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        class_name,
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

/// `KeyPairGenerator.generateKeyPair` contract for a recognised-but-unimplemented
/// algorithm.
fn throw_no_such_algorithm(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    throw_jca(ctx, "java/security/NoSuchAlgorithmException", msg)
}

/// `KeyFactory.generatePublic` / `generatePrivate` contract when we cannot turn
/// the given `KeySpec` into a usable key (parse failure, or no implementation).
fn throw_invalid_key_spec(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    throw_jca(ctx, "java/security/spec/InvalidKeySpecException", msg)
}

// ---------------------------------------------------------------------------
// Post-quantum (ML-DSA / ML-KEM) → real JDK SPI routing (crate::route_pqc_to_real)
// ---------------------------------------------------------------------------
//
// CratonVM has no native ML-DSA/ML-KEM lattice crypto, but JDK 25 ships real
// pure-Java implementations: ML-DSA in the SUN provider
// (`sun.security.provider.ML_DSA_Impls`) and ML-KEM in SunJCE
// (`com.sun.crypto.provider.ML_KEM_Impls`). Both use the JDK "Named" SPI
// framework with one concrete `KeyFactorySpi`/`KeyPairGeneratorSpi` subclass per
// parameter set, suffixed by NIST security category (2/3/5). We drive those SPIs
// the same way `drive_real_ec_*` drives `sun.security.ec.*`, so keycloak's AKP
// JWK parsing (`KeyFactory.getInstance("ML-DSA-44").generatePublic(spec)`) and
// keypair generation yield real, HotSpot-equivalent keys instead of the
// synthetic stub's `NoSuchAlgorithmException`/`InvalidKeySpecException`.

/// `(keypairgen_spi_class, keyfactory_spi_class)` for an ML-DSA/ML-KEM algo
/// index, or `None` for any non-PQC algorithm. Suffix mapping verified against
/// the JDK 25 SPI constructors (e.g. `ML_DSA_Impls$KF2` → "ML-DSA-44").
fn pqc_spi_classes(algo: i32) -> Option<(String, String)> {
    let (base, suffix) = match algo {
        0 => ("com/sun/crypto/provider/ML_KEM_Impls", "2"), // ML-KEM-512
        1 => ("com/sun/crypto/provider/ML_KEM_Impls", "3"), // ML-KEM-768
        2 => ("com/sun/crypto/provider/ML_KEM_Impls", "5"), // ML-KEM-1024
        3 => ("sun/security/provider/ML_DSA_Impls", "2"),   // ML-DSA-44
        4 => ("sun/security/provider/ML_DSA_Impls", "3"),   // ML-DSA-65
        5 => ("sun/security/provider/ML_DSA_Impls", "5"),   // ML-DSA-87
        _ => return None,
    };
    Some((format!("{base}$KPG{suffix}"), format!("{base}$KF{suffix}")))
}

/// Drive the real JDK PQC `KeyPairGenerator` SPI: `new KPG<n>()` →
/// `generateKeyPair()`. `NamedKeyPairGenerator.generateKeyPair()` self-seeds
/// from `JCAUtil.getDefSecureRandom()` when uninitialized (which works under
/// CratonVM), so no explicit `initialize` is needed. Returns a real
/// `java.security.KeyPair` (privateKey@0, publicKey@1).
///
/// This is only correct because the native `SHA3.keccak` override (lib.rs)
/// makes SHAKE256 produce real output; without it the JDK lattice keygen yields
/// degenerate all-zero keys.
fn drive_real_pqc_keypair(ctx: &mut dyn NativeContext, algo: i32) -> MethodCallResult {
    let kpg_class = match pqc_spi_classes(algo) {
        Some((kpg, _)) => kpg,
        None => {
            return Err(throw_no_such_algorithm(
                ctx,
                &format!("{} KeyPairGenerator not available", algo_name(algo)),
            ))
        }
    };
    let spi = match ctx.new_object_initialized(&kpg_class, "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => {
            other?;
            return Err(throw_no_such_algorithm(
                ctx,
                &format!("{} KeyPairGenerator not available", algo_name(algo)),
            ));
        }
    };
    // A real PQC provider returns a genuine KeyPair here. If the SPI yields
    // null/None (e.g. the provider is unavailable, as in a unit-test mock), fail
    // CLOSED — never hand back a null KeyPair, which presents failed keygen as
    // success (no-synthetic-stubs policy; same contract the RSA/EC paths honour).
    match ctx.invoke_virtual(spi, "generateKeyPair", "()Ljava/security/KeyPair;", &[])? {
        Some(Value::Object(Some(kp))) => Ok(Some(Value::Object(Some(kp)))),
        _ => Err(throw_no_such_algorithm(
            ctx,
            &format!("{} KeyPairGenerator produced no key", algo_name(algo)),
        )),
    }
}

/// Drive the real JDK PQC `KeyFactory` SPI's `engineGenerate{Public,Private}`
/// over `spec`. `method`/`ret` select public vs private. Pins `spec` across the
/// SPI allocation (which can GC).
fn drive_real_pqc_keyfactory(
    ctx: &mut dyn NativeContext,
    algo: i32,
    spec: ObjectRef,
    method: &str,
    ret: &str,
) -> MethodCallResult {
    let kf_class = match pqc_spi_classes(algo) {
        Some((_, kf)) => kf,
        None => return Err(throw_invalid_key_spec(ctx, "not a post-quantum algorithm")),
    };
    let spec_pin = ctx.pin_native_root(spec);
    let spi = match ctx.new_object_initialized(&kf_class, "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => {
            ctx.unpin_native_roots(spec_pin);
            other?;
            return Err(throw_invalid_key_spec(
                ctx,
                &format!("{} KeyFactory not available", algo_name(algo)),
            ));
        }
    };
    let spec = ctx.read_native_pin(spec_pin, spec);
    let desc = format!("(Ljava/security/spec/KeySpec;){ret}");
    let r = ctx.invoke_virtual(spi, method, &desc, &[Value::Object(Some(spec))]);
    ctx.unpin_native_roots(spec_pin);
    // Fail CLOSED if the SPI produced no key (null/None) — e.g. the PQC provider
    // is unavailable (unit-test mock). Returning a null key would silently pass
    // off a dead key as success.
    match r? {
        Some(Value::Object(Some(k))) => Ok(Some(Value::Object(Some(k)))),
        _ => Err(throw_invalid_key_spec(
            ctx,
            &format!("{} KeyFactory produced no key", algo_name(algo)),
        )),
    }
}

// ---------------------------------------------------------------------------
// KeyPairGenerator natives
// ---------------------------------------------------------------------------

fn kpg_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let alg = read_string(ctx, args, 0);
    let idx = algo_idx(&alg);
    // Resolve the requested provider BEFORE allocating the synthetic (the
    // Provider.getName() invoke can trigger GC, which would relocate the KPG and
    // desync its raw-ObjectRef side-table entries).
    let provider_name = requested_provider_name(ctx, args);

    // BC-FIPS registers EC keypair generators through its Provider-owned
    // EngineCreator map. Return that real KeyPairGenerator object directly so
    // initialize()/generateKeyPair() run BC-FIPS bytecode instead of falling
    // through to the SunEC shortcut used for default EC.
    if idx == ALGO_EC && is_bc_fips_provider(&provider_name) {
        if let Some(result) =
            super::provider_chain::build_jca_impl(ctx, &provider_name, "KeyPairGenerator", &alg)
        {
            return result;
        }
        return Err(throw_no_such_algorithm(
            ctx,
            &format!("no KeyPairGenerator {alg} implementation for provider {provider_name}"),
        ));
    }

    let is_bc = is_bc_provider(&provider_name);
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let kpg = try_alloc_concurrent_synthetic(
        ctx,
        "java/security/KeyPairGenerator",
        base + KPG_PRIVATE_SLOTS,
    )?;
    // SigProbe fix: the JDK 25 `KeyPairGenerator` class declares
    // `String algorithm` at the inherited `KeyPairGeneratorSpi` layout
    // boundary. The side table (keyed on the receiver ObjectRef) carries
    // the algorithm index reliably across the call chain, mirroring the
    // proven pattern in `message_digest::accumulators`.
    set_kpg_algo(ctx, kpg, idx);
    set_kpg_name(ctx, kpg, alg.clone());
    // Record a BouncyCastle provider request (getInstance(alg, "BC"|BCprovider))
    // so EC keygen can hand out genuine BC keys (see `kpg_bcprov_table`).
    set_kpg_bcprov(ctx, kpg, is_bc);
    let default_bits = if idx == ALGO_RSA || idx == ALGO_DSA {
        2048
    } else if idx == ALGO_EC {
        256
    } else {
        0
    };
    set_kpg_keysize(ctx, kpg, default_bits);
    // Also write the algorithm string to the real-JDK named field so the
    // bytecode-side `getAlgorithm()` (if ever reached on this receiver)
    // sees the expected value.
    let algo_str = ctx.create_string(&alg);
    ctx.set_field_by_name(kpg, "algorithm", Value::Object(Some(algo_str)));
    // Base-offset slot path: appended past the real layout, so these
    // writes are a reliable secondary store for synthetic-mode callers.
    ctx.set_field(kpg, base + KPG_OFF_ALGO, Value::Int(idx));
    ctx.set_field(kpg, base + KPG_OFF_KEYSIZE, Value::Int(default_bits));
    ctx.set_field(kpg, base + KPG_OFF_STATE, Value::Int(0));
    Ok(Some(Value::Object(Some(kpg))))
}

fn kpg_initialize_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let bits = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 2048,
    };
    // The real JDK rejects a nonsensical size with `InvalidParameterException`
    // (a subclass of `IllegalArgumentException`, which is the closest
    // `RuntimeError` variant) rather than pretending to configure the
    // generator. Carried over from the retired `phases_early` KeyPairGenerator
    // stub, which was the only place this check lived.
    if bits <= 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Invalid key size: {bits}"),
        }
        .into());
    }
    set_kpg_keysize(ctx, this, bits);
    ctx.set_field(this, base + KPG_OFF_KEYSIZE, Value::Int(bits));
    ctx.set_field(this, base + KPG_OFF_STATE, Value::Int(1));
    Ok(None)
}

fn kpg_initialize_int_random(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    kpg_initialize_int(ctx, args)
}

fn kpg_initialize_spec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // ECGenParameterSpec / RSAKeyGenParameterSpec. RSA spec keysize is preserved
    // from the previous value / the default. For EC, stash the spec so the real
    // keygen drive (`drive_real_ec_keypair`) honours the requested curve.
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let cur = get_kpg_keysize(ctx, this).unwrap_or_else(|| {
        match ctx.get_field(this, base + KPG_OFF_KEYSIZE) {
            Value::Int(n) => n,
            _ => 0,
        }
    });
    let algo =
        get_kpg_algo(ctx, this).unwrap_or_else(|| match ctx.get_field(this, base + KPG_OFF_ALGO) {
            Value::Int(i) => i,
            _ => -1,
        });
    if let Some(Value::Object(Some(spec))) = args.get(1) {
        ctx.set_field(this, base + KPG_OFF_SPEC, Value::Object(Some(*spec)));
    }
    let bits = if algo == ALGO_EC {
        256
    } else if cur == 0 {
        2048
    } else {
        cur
    };
    set_kpg_keysize(ctx, this, bits);
    ctx.set_field(this, base + KPG_OFF_KEYSIZE, Value::Int(bits));
    ctx.set_field(this, base + KPG_OFF_STATE, Value::Int(1));
    Ok(None)
}

fn kpg_initialize_spec_random(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    kpg_initialize_spec(ctx, args)
}

fn kpg_generate_key_pair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    // SigProbe fix: prefer the side-table read (survives real-JDK class
    // layouts where slot 0 collides with an inherited Object field).
    let algo = get_kpg_algo(ctx, this).or_else(|| match ctx.get_field(this, base + KPG_OFF_ALGO) {
        Value::Int(i) => Some(i),
        _ => None,
    });
    let algo = match algo {
        Some(i) => i,
        None => {
            return Err(RuntimeError::NotImplemented {
                feature: "KeyPairGenerator with no algorithm".into(),
            }
            .into())
        }
    };
    let bits = get_kpg_keysize(ctx, this)
        .filter(|n| *n > 0)
        .or_else(|| match ctx.get_field(this, base + KPG_OFF_KEYSIZE) {
            Value::Int(n) if n > 0 => Some(n),
            _ => None,
        })
        .map(|n| n as usize)
        .unwrap_or(2048);

    if algo == ALGO_RSA {
        // Fast Rust keygen (the actual optimisation — no slow interpreter prime
        // generation). The resulting components + crypto material are real.
        let (pk, sk) = crypto_impl::Rsa::generate_keypair(bits);
        let n_bytes = pk.n.to_bytes_be();
        let e_bytes = pk.e.to_bytes_be();
        let d_bytes = sk.d.to_bytes_be();
        // Extract the CRT parameter magnitudes BEFORE `sk` is moved into the
        // key store, so the real private key can be built as a full CRT
        // `RSAPrivateCrtKeyImpl` (complete `getEncoded()`). Every generated key
        // carries these; `and_then` yields `None` only for the (never-hit here)
        // non-CRT case, which falls back to the legacy 2-arg key.
        let crt_bytes: Option<[Vec<u8>; 5]> = match (&sk.p, &sk.q, &sk.dp, &sk.dq, &sk.qinv) {
            (Some(p), Some(q), Some(dp), Some(dq), Some(qinv)) => Some([
                p.to_bytes_be(),
                q.to_bytes_be(),
                dp.to_bytes_be(),
                dq.to_bytes_be(),
                qinv.to_bytes_be(),
            ]),
            _ => None,
        };
        let pk_der = crypto_impl::Rsa::public_key_to_der(&pk);
        let sk_der = crypto_impl::Rsa::private_key_to_der(&sk);
        let key_id = crypto_impl::rsa_key_next_id();
        crypto_impl::rsa_key_store(
            key_id,
            crypto_impl::RsaKeyPairData {
                public_key: pk,
                private_key: sk,
            },
        );
        // Default: hand out GENUINE RSAPublic/PrivateKeyImpl objects (re-imported
        // from our own components via the real KeyFactory) so `(RSAPublicKey) k`
        // casts, `getModulus()`, real `getEncoded()` and BC cert generation all
        // work — while sign/verify stay on the fast crypto_impl path via the
        // identity bridge. CRATONVM_SYNTHETIC_RSA=1 restores the bare-interface
        // synthetic keys (faster alloc, but the cast/cert paths fail).
        if crate::route_rsa_to_real()
            && real_spi_available(ctx, "sun/security/rsa/RSAKeyFactory$Legacy")
        {
            let crt_ref: Option<[&[u8]; 5]> = crt_bytes.as_ref().map(|a| {
                [
                    a[0].as_slice(),
                    a[1].as_slice(),
                    a[2].as_slice(),
                    a[3].as_slice(),
                    a[4].as_slice(),
                ]
            });
            if let Ok(kp) = real_rsa_keypair(ctx, &n_bytes, &e_bytes, &d_bytes, crt_ref, key_id) {
                return Ok(Some(Value::Object(Some(kp))));
            }
            // Fall through to the synthetic keys if the real SPI is unavailable.
        }
        let pub_obj = alloc_public_key(ctx, ALGO_RSA, bits as i32, &pk_der, key_id);
        let priv_obj = alloc_private_key(ctx, ALGO_RSA, bits as i32, &sk_der, key_id);
        return Ok(Some(Value::Object(Some(alloc_keypair(
            ctx, pub_obj?, priv_obj?,
        )?))));
    }

    if algo == ALGO_EC {
        // Route EC to the real SunEC SPI → concrete ECPublicKey/ECPrivateKey in a
        // real KeyPair (fixes the bare-interface CCE). RSA/AES keep the synthetic
        // path below; CRATONVM_SYNTHETIC_EC=1 restores the legacy synthetic EC.
        // ...but only when the SunEC bytecode is genuinely present. Under the
        // synthetic JDK the drive would target a fabricated, code-less stub and
        // there is no fallback after it, so check first (see
        // `real_ec_keypair_available`).
        if crate::route_ec_to_real() && real_ec_keypair_available(ctx, this) {
            return drive_real_ec_keypair(ctx, this);
        }
        let (pk, sk) = crypto_impl::Ecdsa::generate_keypair();
        let pk_der = crypto_impl::Ecdsa::public_key_to_der(&pk);
        let sk_bytes = crypto_impl::Ecdsa::private_key_to_bytes(&sk);
        let key_id = crypto_impl::ecdsa_key_next_id();
        crypto_impl::ecdsa_key_store(
            key_id,
            crypto_impl::EcdsaKeyPairData {
                public_key: pk,
                private_key: sk,
            },
        );
        let pub_obj = alloc_public_key(ctx, ALGO_EC, 256, &pk_der, key_id);
        let priv_obj = alloc_private_key(ctx, ALGO_EC, 256, &sk_bytes, key_id);
        return Ok(Some(Value::Object(Some(alloc_keypair(
            ctx, pub_obj?, priv_obj?,
        )?))));
    }

    if algo == ALGO_DSA && crate::route_dsa_to_real() {
        return drive_real_dsa_keypair(ctx, this);
    }

    if crate::route_ec_to_real() && matches!(algo, ALGO_ED25519 | ALGO_ED448) {
        return drive_real_eddsa_keypair(ctx, algo);
    }

    // ML-DSA / ML-KEM: drive the real JDK 25 PQC KeyPairGenerator SPI (SUN /
    // SunJCE) for a genuine, HotSpot-equivalent keypair. Safe now that the
    // native `SHA3.keccak` override makes SHAKE256 produce real output (without
    // it the JDK lattice keygen yields degenerate all-zero keys).
    if crate::route_pqc_to_real() && pqc_spi_classes(algo).is_some() {
        return drive_real_pqc_keypair(ctx, algo);
    }

    // Recognised but not implemented (Ed25519, X25519, an unknown name, or PQC
    // with routing disabled). Real key generation is unavailable, so honour the
    // JDK contract and throw `NoSuchAlgorithmException` rather than minting a
    // KeyPair with empty key material (no-synthetic-stubs policy). RSA and EC
    // returned above with real keys.
    let requested = get_kpg_name(ctx, this).unwrap_or_else(|| algo_name(algo).to_string());
    Err(throw_no_such_algorithm(
        ctx,
        &format!("{requested} KeyPairGenerator not available"),
    ))
}

fn kpg_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyPairGenerator");
    let idx = match ctx.get_field(this, base + KPG_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    };
    let name = get_kpg_name(ctx, this).unwrap_or_else(|| algo_name(idx).to_string());
    let s = ctx.create_string(&name);
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// KeyFactory natives
// ---------------------------------------------------------------------------

fn kf_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // All three `getInstance` overloads share this native, so the
    // `(algorithm, String provider)` form's provider argument has to be
    // validated here — real JDK resolves the provider before the algorithm.
    crate::jca::provider_chain::check_named_provider_arg(
        ctx,
        args,
        1,
        crate::jca::provider_chain::ProviderArgWording::Shared,
    )?;
    let alg = read_string(ctx, args, 0);
    crate::jca::provider_chain::check_provider_ownership(
        ctx,
        args,
        1,
        "KeyFactory",
        &alg,
        crate::jca::provider_chain::ProviderArgWording::Shared,
    )?;
    let idx = kf_algo_idx(&alg);
    // `KeyFactory.getInstance` must reject an unrecognised name. In
    // particular, `X509Key.buildX509Key` deliberately catches
    // `NoSuchAlgorithmException` and falls back to a generic `X509Key` for
    // certificates with an unknown SubjectPublicKeyInfo OID. Returning a
    // synthetic factory with `Unknown` state instead makes its later
    // `generatePublic` throw `InvalidKeySpecException`, bypassing that JDK
    // fallback and rejecting certificates that HotSpot accepts.
    if idx < 0 {
        return Err(throw_no_such_algorithm(
            ctx,
            &format!("{alg} KeyFactory not available"),
        ));
    }
    let base = synthetic_base_offset(ctx, "java/security/KeyFactory");
    let kf = try_alloc_concurrent_synthetic(ctx, "java/security/KeyFactory", base + KF_PRIVATE_SLOTS)?;
    set_kf_algo(ctx, kf, idx);
    ctx.set_field(kf, base + KF_OFF_ALGO, Value::Int(idx));
    Ok(Some(Value::Object(Some(kf))))
}

/// generatePublic(KeySpec) -> PublicKey.  We surface the most common path
/// (X509EncodedKeySpec wrapping a SubjectPublicKeyInfo DER blob) and
/// re-parse via crypto_impl.  EC (default) routes to the real SunEC
/// KeyFactory.  When we cannot produce a usable key — the RSA/EC spec fails
/// to parse, or the algorithm has no real implementation — we throw
/// `InvalidKeySpecException` (generatePublic's declared checked exception)
/// rather than returning a `key_id == 0` key that silently fails every later
/// verify (no-synthetic-stubs policy).
fn kf_generate_public(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyFactory");
    let algo =
        get_kf_algo(ctx, this).unwrap_or_else(|| match ctx.get_field(this, base + KF_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    });
    // EdDSA / XDH: reconstruct concrete SunEC Ed25519/Ed448/X25519/X448 keys
    // from the standard EdECPublicKeySpec/XECPublicKeySpec/X509EncodedKeySpec.
    // Keycloak builds an EdECPublicKeySpec while importing OKP JWKs; Spring
    // Boot's PemPrivateKeyParser always requests the GENERIC "EdDSA"/"XDH"
    // names (curve sniffed from the spec's own OID — see
    // `drive_eddsa_or_xdh_keyfactory`). Leaving this on the synthetic path
    // made every curve throw InvalidKeySpecException despite working
    // key-pair generation.
    if let Some(Value::Object(Some(spec))) = args.get(1) {
        if let Some(result) = drive_eddsa_or_xdh_keyfactory(
            ctx,
            algo,
            *spec,
            "engineGeneratePublic",
            "Ljava/security/PublicKey;",
        ) {
            match result {
                Ok(Some(key)) => return Ok(Some(key)),
                // A context that cannot execute the real SPI must not turn
                // that absence into a null/dead public key. Fall through to
                // the declared InvalidKeySpecException below.
                Ok(None) => {}
                Err(err) => return Err(err),
            }
        }
    }
    // RSASSA-PSS: a distinct, stricter real SPI than the permissive "RSA" /
    // RSAKeyFactory$Legacy driven below — see ALGO_RSASSA_PSS's doc comment.
    if algo == ALGO_RSASSA_PSS && crate::route_rsa_to_real() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            if let Ok(Some(Value::Object(Some(key)))) = drive_real_rsa_pss_keyfactory(
                ctx,
                *spec,
                "engineGeneratePublic",
                "Ljava/security/PublicKey;",
            ) {
                register_rsa_pub_verify_material(ctx, key);
                return Ok(Some(Value::Object(Some(key))));
            }
        }
    }
    // DSA: drive the real sun.security.provider.DSAKeyFactory SPI (no
    // synthetic DSA key material to fall back to at all). See ALGO_DSA's doc
    // comment for the root-cause story (X509Key.parse() re-parsing a real
    // X.509 cert's DSA SubjectPublicKeyInfo).
    if algo == ALGO_DSA && crate::route_dsa_to_real() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            return drive_real_dsa_keyfactory(
                ctx,
                *spec,
                "engineGeneratePublic",
                "Ljava/security/PublicKey;",
            );
        }
    }
    // EC: drive the real SunEC KeyFactory over the ECPublicKeySpec → real
    // ECPublicKeyImpl (the synthetic path can't honour an ECPublicKeySpec).
    if algo == ALGO_EC && crate::route_ec_to_real() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            let r = drive_real_ec_keyfactory(
                ctx,
                *spec,
                "engineGeneratePublic",
                "Ljava/security/PublicKey;",
            );
            // SunEC only understands `java.security.spec.*`; a BC-specific
            // `org.bouncycastle.jce.spec.ECPublicKeySpec` (keycloak's
            // `BCECDSACryptoProvider.getPublicFromPrivate`) makes it throw
            // InvalidKeySpecException. Retry through BC's own EC KeyFactory SPI,
            // which accepts the BC spec natively and returns a BCECPublicKey.
            if r.is_err() {
                if let Ok(ok @ Some(Value::Object(Some(_)))) = drive_keyspec_spi(
                    ctx,
                    "org/bouncycastle/jcajce/provider/asymmetric/ec/KeyFactorySpi$EC",
                    *spec,
                    "engineGeneratePublic",
                    "Ljava/security/PublicKey;",
                ) {
                    return Ok(ok);
                }
            }
            return r;
        }
    }
    // RSA: drive the real KeyFactory over WHATEVER spec the caller passed —
    // X509EncodedKeySpec OR RSAPublicKeySpec (the synthetic DER-parse path below
    // only understands X509-encoded bytes, so `generatePublic(RSAPublicKeySpec)`
    // otherwise dead-ends → "cannot generate a usable RSA public key"). The real
    // key is then bridged for fast verify via its own X.509 encoding.
    if algo == ALGO_RSA && crate::route_rsa_to_real() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            if let Ok(Some(Value::Object(Some(key)))) = drive_real_rsa_keyfactory(
                ctx,
                *spec,
                "engineGeneratePublic",
                "Ljava/security/PublicKey;",
            ) {
                register_rsa_pub_verify_material(ctx, key);
                return Ok(Some(Value::Object(Some(key))));
            }
        }
    }
    let der = if let Some(Value::Object(Some(spec))) = args.get(1) {
        // X509EncodedKeySpec.encoded[B is at field 0 in the synthetic
        // KeySpec shape; for real-JDK KeySpec we read field 0 too —
        // both place the encoded-key byte[] first.
        match ctx.get_field(*spec, 0) {
            Value::Object(Some(arr)) => read_byte_array(ctx, arr),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };

    if algo == ALGO_RSA {
        // Accept either an X509 `SubjectPublicKeyInfo` DER (parsed above) OR an
        // `RSAPublicKeySpec` (modulus/publicExponent). The spec branch is the
        // synthetic-mode path; in real mode the drive at the top of this fn has
        // already returned a genuine `RSAPublicKeyImpl` for the spec.
        let mut pk_opt = crypto_impl::parse_rsa_public_key(&der);
        // Only consult the spec's BigInteger getters when there is NO encoded
        // DER — an `RSAPublicKeySpec` carries `modulus`/`publicExponent` objects
        // (field 0 is not a byte[], so `der` is empty), whereas an
        // `X509EncodedKeySpec` always has its bytes at field 0. This avoids
        // calling `getModulus()` on a non-RSAPublicKeySpec.
        if pk_opt.is_none() && der.is_empty() {
            if let Some(Value::Object(Some(spec))) = args.get(1) {
                let spec = *spec;
                if let Some((n, e)) = rsa_pubspec_components(ctx, spec) {
                    pk_opt = Some(crypto_impl::RsaPublicKey {
                        n: crypto_impl::BigUint::from_bytes_be(&n),
                        e: crypto_impl::BigUint::from_bytes_be(&e),
                    });
                }
            }
        }
        if let Some(pk) = pk_opt {
            let n_bytes = pk.n.to_bytes_be();
            let e_bytes = pk.e.to_bytes_be();
            let pk_der = crypto_impl::Rsa::public_key_to_der(&pk);
            // We can't honour an external private-key counterpart from a
            // public-only spec, so we register a public-only handle by
            // pairing with a placeholder private (only sign uses private).
            // Wave 6 leaves verify-only flows fully functional.
            let key_id = crypto_impl::rsa_key_next_id();
            // Manufacture a placeholder paired keypair so the registry
            // entry exists; sign attempts on this id will use the
            // placeholder private key and produce wrong bytes — which is
            // fine because callers only verify with public-import paths.
            crypto_impl::rsa_key_store(
                key_id,
                crypto_impl::RsaKeyPairData {
                    public_key: pk,
                    // Dummy private — never used because public-only
                    // imports always go through verify.
                    private_key: crypto_impl::RsaPrivateKey {
                        n: crypto_impl::BigUint::from_bytes_be(&[1]),
                        d: crypto_impl::BigUint::from_bytes_be(&[1]),
                        e: crypto_impl::BigUint::from_bytes_be(&[1]),
                        p: None,
                        q: None,
                        dp: None,
                        dq: None,
                        qinv: None,
                    },
                },
            );
            // Default: real RSAPublicKeyImpl (fixes `(RSAPublicKey)` casts and
            // BC consumers); verify stays on the fast crypto_impl path via the
            // identity bridge. CRATONVM_SYNTHETIC_RSA=1 → bare-interface key.
            if crate::route_rsa_to_real() {
                if let Ok(key) = real_rsa_key_from_components(ctx, &n_bytes, &e_bytes, key_id, true)
                {
                    return Ok(Some(Value::Object(Some(key))));
                }
            }
            return Ok(Some(Value::Object(Some(alloc_public_key(
                ctx, ALGO_RSA, 2048, &pk_der, key_id,
            )?))));
        }
    }
    if algo == ALGO_EC {
        if let Some(pk) = crypto_impl::parse_ecdsa_public_key(&der) {
            let pk_der = crypto_impl::Ecdsa::public_key_to_der(&pk);
            let key_id = crypto_impl::ecdsa_key_next_id();
            // Placeholder private key.
            let (_, sk) = crypto_impl::Ecdsa::generate_keypair();
            crypto_impl::ecdsa_key_store(
                key_id,
                crypto_impl::EcdsaKeyPairData {
                    public_key: pk,
                    private_key: sk,
                },
            );
            return Ok(Some(Value::Object(Some(alloc_public_key(
                ctx, ALGO_EC, 256, &pk_der, key_id,
            )?))));
        }
    }

    // ML-DSA / ML-KEM: drive the real JDK 25 PQC KeyFactory SPI over the
    // X509EncodedKeySpec → real public key (keycloak AKP JWK parsing path).
    if crate::route_pqc_to_real() && pqc_spi_classes(algo).is_some() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            return drive_real_pqc_keyfactory(
                ctx,
                algo,
                *spec,
                "engineGeneratePublic",
                "Ljava/security/PublicKey;",
            );
        }
    }

    // No usable key could be produced: the RSA/EC spec failed to parse, or the
    // algorithm has no real implementation (Ed25519, X25519, an unknown name,
    // or PQC with routing disabled). Throw generatePublic's declared
    // `InvalidKeySpecException` rather than returning a `key_id == 0` key that
    // silently fails every later verify (no-synthetic-stubs policy).
    Err(throw_invalid_key_spec(
        ctx,
        &format!(
            "cannot generate a usable {} public key from the given KeySpec",
            algo_name(algo)
        ),
    ))
}

/// generatePrivate(KeySpec) -> PrivateKey.  Only the real SunEC EC path can
/// import a usable private key; `crypto_impl` has no other private-key parser,
/// so every other algorithm would otherwise yield a `key_id == 0` key that can
/// never sign.  Honour generatePrivate's declared `InvalidKeySpecException`
/// rather than returning that unusable synthetic key (no-synthetic-stubs).
fn kf_generate_private(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyFactory");
    let algo =
        get_kf_algo(ctx, this).unwrap_or_else(|| match ctx.get_field(this, base + KF_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    });
    // EdDSA / XDH: mirrors `kf_generate_public`'s route (curve-specific or
    // generic "EdDSA"/"XDH", curve sniffed from the spec's own OID when
    // generic). This is the private-key half of the PemPrivateKeyParser fix
    // — PKCS8 EdDSA/XDH import previously had NO route here at all (unlike
    // generatePublic's pre-existing EdECPublicKeySpec support), so every
    // Ed25519/Ed448/X25519/X448 PEM private key fell straight through to the
    // InvalidKeySpecException below.
    if let Some(Value::Object(Some(spec))) = args.get(1) {
        if let Some(result) = drive_eddsa_or_xdh_keyfactory(
            ctx,
            algo,
            *spec,
            "engineGeneratePrivate",
            "Ljava/security/PrivateKey;",
        ) {
            match result {
                Ok(Some(key)) => return Ok(Some(key)),
                Ok(None) => {}
                Err(err) => return Err(err),
            }
        }
    }
    // RSASSA-PSS: see ALGO_RSASSA_PSS's doc comment — a distinct, stricter
    // real SPI than the permissive "RSA" driven below.
    if algo == ALGO_RSASSA_PSS {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            if let Ok(r) = drive_real_rsa_pss_keyfactory(
                ctx,
                *spec,
                "engineGeneratePrivate",
                "Ljava/security/PrivateKey;",
            ) {
                if let Some(Value::Object(Some(key))) = r {
                    register_rsa_priv_sign_material(ctx, key);
                    return Ok(Some(Value::Object(Some(key))));
                }
            }
        }
    }
    // DSA: drive the real sun.security.provider.DSAKeyFactory SPI. See
    // ALGO_DSA's doc comment / `kf_generate_public` for the root-cause story.
    if algo == ALGO_DSA && crate::route_dsa_to_real() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            return drive_real_dsa_keyfactory(
                ctx,
                *spec,
                "engineGeneratePrivate",
                "Ljava/security/PrivateKey;",
            );
        }
    }
    // EC: drive the real SunEC KeyFactory over the ECPrivateKeySpec → real
    // ECPrivateKeyImpl (the only private-key import we can satisfy).
    if algo == ALGO_EC && crate::route_ec_to_real() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            let r = drive_real_ec_keyfactory(
                ctx,
                *spec,
                "engineGeneratePrivate",
                "Ljava/security/PrivateKey;",
            );
            // BC-specific `org.bouncycastle.jce.spec.ECPrivateKeySpec` → BC SPI.
            if r.is_err() {
                if let Ok(ok @ Some(Value::Object(Some(_)))) = drive_keyspec_spi(
                    ctx,
                    "org/bouncycastle/jcajce/provider/asymmetric/ec/KeyFactorySpi$EC",
                    *spec,
                    "engineGeneratePrivate",
                    "Ljava/security/PrivateKey;",
                ) {
                    return Ok(ok);
                }
            }
            return r;
        }
    }
    // ML-DSA / ML-KEM: drive the real JDK 25 PQC KeyFactory SPI over the
    // PKCS8EncodedKeySpec → real private key.
    if crate::route_pqc_to_real() && pqc_spi_classes(algo).is_some() {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            return drive_real_pqc_keyfactory(
                ctx,
                algo,
                *spec,
                "engineGeneratePrivate",
                "Ljava/security/PrivateKey;",
            );
        }
    }
    // RSA: drive the real SunRsaSign RSAKeyFactory over the spec → a genuine
    // RSAPrivate{Crt}KeyImpl holding the spec's real components (the spec
    // already carries the modulus/exponents/CRT factors, so this is a real key
    // import, not a synthetic stub). Falls back to InvalidKeySpecException if
    // the real SPI is unavailable.
    if algo == ALGO_RSA {
        if let Some(Value::Object(Some(spec))) = args.get(1) {
            // Try the spec as-is (PKCS8EncodedKeySpec / RSAPrivateKeySpec).
            if let Ok(r) = drive_real_rsa_keyfactory(
                ctx,
                *spec,
                "engineGeneratePrivate",
                "Ljava/security/PrivateKey;",
            ) {
                if let Some(Value::Object(Some(key))) = r {
                    register_rsa_priv_sign_material(ctx, key);
                    return Ok(Some(Value::Object(Some(key))));
                }
                // r was None (real SPI unavailable / produced no key) — do NOT
                // return a null PrivateKey; fall through to the PKCS#1 retry and
                // ultimately the InvalidKeySpecException below (fail closed).
            }
            // Fallback: the encoded spec may carry a PKCS#1 *traditional*
            // RSAPrivateKey rather than a PKCS#8 PrivateKeyInfo. BouncyCastle's
            // RSA KeyFactory accepts PKCS#1 directly (and BC writes RSA keys as
            // "RSA PRIVATE KEY" PEM), and keycloak's DerUtils.decodePrivateKey
            // relies on that leniency — but the strict SunRsaSign SPI we route
            // to rejects it. Detect PKCS#1, wrap into PKCS#8, and retry.
            let der = match ctx.get_field(*spec, 0) {
                Value::Object(Some(arr)) => read_byte_array(ctx, arr),
                _ => Vec::new(),
            };
            if is_pkcs1_rsa_private(&der) {
                let pkcs8 = rsa_pkcs1_to_pkcs8(&der);
                let arr = alloc_byte_array(ctx, &pkcs8);
                if let Ok(Some(Value::Object(Some(new_spec)))) = ctx.new_object_initialized(
                    "java/security/spec/PKCS8EncodedKeySpec",
                    "([B)V",
                    &[Value::Object(Some(arr))],
                ) {
                    if let Ok(r) = drive_real_rsa_keyfactory(
                        ctx,
                        new_spec,
                        "engineGeneratePrivate",
                        "Ljava/security/PrivateKey;",
                    ) {
                        if let Some(Value::Object(Some(key))) = r {
                            register_rsa_priv_sign_material(ctx, key);
                            return Ok(Some(Value::Object(Some(key))));
                        }
                        // None — fall through to InvalidKeySpecException below.
                    }
                }
            }
        }
    }
    Err(throw_invalid_key_spec(
        ctx,
        &format!(
            "cannot generate a usable {} private key from the given KeySpec",
            algo_name(algo)
        ),
    ))
}

/// Number of bytes occupied by a DER length field at `der[pos]`.
fn der_len_size(der: &[u8], pos: usize) -> usize {
    match der.get(pos) {
        Some(&b) if b < 0x80 => 1,
        Some(&b) => 1 + (b & 0x7f) as usize,
        None => 1,
    }
}

/// True if `der` is a PKCS#1 `RSAPrivateKey` (`SEQUENCE { INTEGER version,
/// INTEGER modulus, ... }`) rather than a PKCS#8 `PrivateKeyInfo` (`SEQUENCE {
/// INTEGER version, SEQUENCE algId, OCTET STRING }`). Both start with the
/// version `02 01 00`; the element after it is an INTEGER (`0x02`, the modulus)
/// for PKCS#1 vs a SEQUENCE (`0x30`, the AlgorithmIdentifier) for PKCS#8.
fn is_pkcs1_rsa_private(der: &[u8]) -> bool {
    if der.first() != Some(&0x30) {
        return false;
    }
    let cs = 1 + der_len_size(der, 1); // outer SEQUENCE content start
    der.get(cs) == Some(&0x02)
        && der.get(cs + 1) == Some(&0x01)
        && der.get(cs + 2) == Some(&0x00)
        && der.get(cs + 3) == Some(&0x02)
}

/// Append a DER definite-length encoding of `len` to `out`.
fn der_push_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let mut bytes = Vec::new();
        let mut l = len;
        while l > 0 {
            bytes.push((l & 0xff) as u8);
            l >>= 8;
        }
        bytes.reverse();
        out.push(0x80 | bytes.len() as u8);
        out.extend_from_slice(&bytes);
    }
}

/// Wrap a PKCS#1 `RSAPrivateKey` DER into a PKCS#8 `PrivateKeyInfo` carrying the
/// `rsaEncryption` AlgorithmIdentifier, so the strict SunRsaSign KeyFactory
/// accepts it (it only parses PKCS#8).
fn rsa_pkcs1_to_pkcs8(pkcs1: &[u8]) -> Vec<u8> {
    // AlgorithmIdentifier rsaEncryption: SEQUENCE { OID 1.2.840.113549.1.1.1, NULL }
    const ALG_ID: &[u8] = &[
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
    ];
    // OCTET STRING { pkcs1 }
    let mut octet = vec![0x04];
    der_push_len(&mut octet, pkcs1.len());
    octet.extend_from_slice(pkcs1);
    // PrivateKeyInfo SEQUENCE { INTEGER 0, AlgorithmIdentifier, OCTET STRING }
    let mut inner = vec![0x02, 0x01, 0x00]; // version v1 (0)
    inner.extend_from_slice(ALG_ID);
    inner.extend_from_slice(&octet);
    let mut out = vec![0x30];
    der_push_len(&mut out, inner.len());
    out.extend_from_slice(&inner);
    out
}

fn kf_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let base = synthetic_base_offset(ctx, "java/security/KeyFactory");
    let idx = match ctx.get_field(this, base + KF_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    };
    let s = ctx.create_string(algo_name(idx));
    Ok(Some(Value::Object(Some(s))))
}

/// `KeyFactory.translateKey(Key)` -- the real JDK-25 bytecode is simply
/// `return spi.engineTranslateKey(key);` (`KeyFactory.java:475`), but our
/// synthetic `KeyFactory` never populates the real `spi` field (see the
/// module doc comment), so ANY caller reaching this method NPEs on
/// `this.spi` even though every other `KeyFactory` method here already has
/// a native override. Real providers' `engineTranslateKey` (SunRsaSign
/// `RSAKeyFactory`, SunEC `ECKeyFactory`) special-case "key already belongs
/// to this algorithm's own impl classes" as a pass-through, and otherwise
/// re-derive the key from its public accessor interface
/// (`RSAPublicKey`/`ECPrivateKey` etc.) for a foreign implementation of the
/// SAME algorithm, or throw `InvalidKeyException` for a mismatched one.
/// Mirror that: our `generateKeyPair`/`generatePublic`/`generatePrivate`
/// already hand back real, provider-native key objects (`RSAPrivate/
/// PublicKeyImpl`, `EC*Impl`, BouncyCastle's own classes, or -- for
/// unimplemented algorithms -- our own synthetic `PublicKey`/`PrivateKey`
/// proxies), so there is never a foreign representation left to re-derive:
/// a same-algorithm key passes through unchanged; a mismatched one throws.
fn kf_translate_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(throw_jca(
                ctx,
                "java/security/InvalidKeyException",
                "Key must not be null",
            ))
        }
    };
    let base = synthetic_base_offset(ctx, "java/security/KeyFactory");
    let algo = match ctx.get_field(this, base + KF_OFF_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    };
    let expected = algo_name(algo);
    // Unindexed KeyFactory algorithm (idx == -1, e.g. DSA/DH) -- we have no
    // basis to validate a mismatch, so pass the key through rather than
    // risk a false InvalidKeyException.
    if expected == "Unknown" {
        return Ok(Some(Value::Object(Some(key))));
    }
    let key_algo = match ctx.invoke_virtual(key, "getAlgorithm", "()Ljava/lang/String;", &[])? {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let matches = key_algo.eq_ignore_ascii_case(expected)
        || (algo == ALGO_EC && key_algo.eq_ignore_ascii_case("ECDSA"));
    if !matches {
        return Err(throw_jca(
            ctx,
            "java/security/InvalidKeyException",
            &format!("Key algorithm {key_algo} does not match KeyFactory algorithm {expected}"),
        ));
    }
    Ok(Some(Value::Object(Some(key))))
}

/// `KeyFactory.getKeySpec(Key, Class)` -- the same shim gap as
/// `translateKey` above, surfacing on a sibling method: the real bytecode is
/// `return spi.engineGetKeySpec(key, keySpec);` (`KeyFactory.java:438`), and
/// our synthetic `KeyFactory` never populates `spi`, so this NPEs too.
/// Confirmed via the real Elytron
/// `SelfSignedX509CertificateAndSigningKey.Builder.build()` path (WildFly's
/// `X509CertificateBuilder.getTBSBytes()` calls
/// `keyFactory.getKeySpec(publicKey, X509EncodedKeySpec.class)` to grab the
/// encoded `SubjectPublicKeyInfo`) -- still NPEs here even after
/// `translateKey` alone is fixed. Support the KeySpec classes real
/// SunRsaSign/SunEC `engineGetKeySpec` implementations produce:
/// `X509EncodedKeySpec` / `PKCS8EncodedKeySpec` (from the key's own
/// encoding -- always available, our keys are real provider-native
/// objects), `RSAPublicKeySpec` / `RSAPrivateKeySpec` (from the key's own
/// BigInteger accessors), and `ECPublicKeySpec` / `ECPrivateKeySpec` (from
/// the key's own `getW()`/`getS()` + `getParams()`). Anything else throws
/// `InvalidKeySpecException`, exactly as a real provider would for an
/// unsupported spec class.
fn kf_get_key_spec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let key = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(throw_invalid_key_spec(ctx, "Key must not be null")),
    };
    let spec_class = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(throw_invalid_key_spec(
                ctx,
                "keySpec class must not be null",
            ))
        }
    };
    let spec_class_name =
        match ctx.invoke_virtual(spec_class, "getName", "()Ljava/lang/String;", &[])? {
            Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
    match spec_class_name.as_str() {
        "java.security.spec.X509EncodedKeySpec" => {
            let der = match ctx.invoke_virtual(key, "getEncoded", "()[B", &[])? {
                Some(Value::Object(Some(arr))) => read_byte_array(ctx, arr),
                _ => return Err(throw_invalid_key_spec(ctx, "Key has no X.509 encoding")),
            };
            let arr = alloc_byte_array(ctx, &der);
            ctx.new_object_initialized(
                "java/security/spec/X509EncodedKeySpec",
                "([B)V",
                &[Value::Object(Some(arr))],
            )
        }
        "java.security.spec.PKCS8EncodedKeySpec" => {
            let der = match ctx.invoke_virtual(key, "getEncoded", "()[B", &[])? {
                Some(Value::Object(Some(arr))) => read_byte_array(ctx, arr),
                _ => return Err(throw_invalid_key_spec(ctx, "Key has no PKCS#8 encoding")),
            };
            let arr = alloc_byte_array(ctx, &der);
            ctx.new_object_initialized(
                "java/security/spec/PKCS8EncodedKeySpec",
                "([B)V",
                &[Value::Object(Some(arr))],
            )
        }
        "java.security.spec.RSAPublicKeySpec" => {
            let n = read_biginteger_magnitude(ctx, key, "getModulus");
            let e = read_biginteger_magnitude(ctx, key, "getPublicExponent");
            if n.is_empty() || e.is_empty() {
                return Err(throw_invalid_key_spec(ctx, "Key is not an RSA public key"));
            }
            let n_bi = build_positive_biginteger(ctx, &n)?;
            let pin = ctx.pin_native_root(n_bi);
            let result = (|| {
                let e_bi = build_positive_biginteger(ctx, &e)?;
                let n_bi = ctx.read_native_pin(pin, n_bi);
                ctx.new_object_initialized(
                    "java/security/spec/RSAPublicKeySpec",
                    "(Ljava/math/BigInteger;Ljava/math/BigInteger;)V",
                    &[Value::Object(Some(n_bi)), Value::Object(Some(e_bi))],
                )
            })();
            ctx.unpin_native_roots(pin);
            result
        }
        "java.security.spec.RSAPrivateKeySpec" => {
            let n = read_biginteger_magnitude(ctx, key, "getModulus");
            let d = read_biginteger_magnitude(ctx, key, "getPrivateExponent");
            if n.is_empty() || d.is_empty() {
                return Err(throw_invalid_key_spec(ctx, "Key is not an RSA private key"));
            }
            let n_bi = build_positive_biginteger(ctx, &n)?;
            let pin = ctx.pin_native_root(n_bi);
            let result = (|| {
                let d_bi = build_positive_biginteger(ctx, &d)?;
                let n_bi = ctx.read_native_pin(pin, n_bi);
                ctx.new_object_initialized(
                    "java/security/spec/RSAPrivateKeySpec",
                    "(Ljava/math/BigInteger;Ljava/math/BigInteger;)V",
                    &[Value::Object(Some(n_bi)), Value::Object(Some(d_bi))],
                )
            })();
            ctx.unpin_native_roots(pin);
            result
        }
        "java.security.spec.ECPublicKeySpec" => {
            let pin_key = ctx.pin_native_root(key);
            let result = (|| {
                let key_r = ctx.read_native_pin(pin_key, key);
                let w = match ctx.invoke_virtual(
                    key_r,
                    "getW",
                    "()Ljava/security/spec/ECPoint;",
                    &[],
                )? {
                    Some(Value::Object(Some(o))) => o,
                    _ => return Err(throw_invalid_key_spec(ctx, "Key is not an EC public key")),
                };
                let pin_w = ctx.pin_native_root(w);
                let key_r = ctx.read_native_pin(pin_key, key);
                let params = match ctx.invoke_virtual(
                    key_r,
                    "getParams",
                    "()Ljava/security/spec/ECParameterSpec;",
                    &[],
                )? {
                    Some(Value::Object(Some(o))) => o,
                    _ => return Err(throw_invalid_key_spec(ctx, "Key is not an EC key")),
                };
                let w = ctx.read_native_pin(pin_w, w);
                ctx.new_object_initialized(
                    "java/security/spec/ECPublicKeySpec",
                    "(Ljava/security/spec/ECPoint;Ljava/security/spec/ECParameterSpec;)V",
                    &[Value::Object(Some(w)), Value::Object(Some(params))],
                )
            })();
            ctx.unpin_native_roots(pin_key);
            result
        }
        "java.security.spec.ECPrivateKeySpec" => {
            let pin_key = ctx.pin_native_root(key);
            let result = (|| {
                let key_r = ctx.read_native_pin(pin_key, key);
                let s = match ctx.invoke_virtual(key_r, "getS", "()Ljava/math/BigInteger;", &[])? {
                    Some(Value::Object(Some(o))) => o,
                    _ => return Err(throw_invalid_key_spec(ctx, "Key is not an EC private key")),
                };
                let pin_s = ctx.pin_native_root(s);
                let key_r = ctx.read_native_pin(pin_key, key);
                let params = match ctx.invoke_virtual(
                    key_r,
                    "getParams",
                    "()Ljava/security/spec/ECParameterSpec;",
                    &[],
                )? {
                    Some(Value::Object(Some(o))) => o,
                    _ => return Err(throw_invalid_key_spec(ctx, "Key is not an EC key")),
                };
                let s = ctx.read_native_pin(pin_s, s);
                ctx.new_object_initialized(
                    "java/security/spec/ECPrivateKeySpec",
                    "(Ljava/math/BigInteger;Ljava/security/spec/ECParameterSpec;)V",
                    &[Value::Object(Some(s)), Value::Object(Some(params))],
                )
            })();
            ctx.unpin_native_roots(pin_key);
            result
        }
        _ => Err(throw_invalid_key_spec(
            ctx,
            &format!("Unsupported key spec: {spec_class_name}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// KeyPair / Key accessors
// ---------------------------------------------------------------------------

fn keypair_get_public(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let slot0 = ctx.get_field(this, 0);
    // Our synthetic KeyPair stores publicKey@0 (a synthetic java/security/PublicKey).
    // A *real* java.security.KeyPair (from real EC keygen or `new KeyPair(pub,priv)`)
    // lays out privateKey@0, publicKey@1 — so for it the public key is at slot 1.
    if (crate::route_ec_to_real() || crate::route_rsa_to_real())
        && !is_synthetic_key_obj(ctx, &slot0, "java/security/PublicKey")
    {
        return Ok(Some(ctx.get_field(this, 1)));
    }
    Ok(Some(slot0))
}

fn keypair_get_private(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let slot1 = ctx.get_field(this, 1);
    // Synthetic KeyPair: privateKey@1 (synthetic java/security/PrivateKey). A real
    // java.security.KeyPair has publicKey@1, privateKey@0.
    if (crate::route_ec_to_real() || crate::route_rsa_to_real())
        && !is_synthetic_key_obj(ctx, &slot1, "java/security/PrivateKey")
    {
        return Ok(Some(ctx.get_field(this, 0)));
    }
    Ok(Some(slot1))
}

fn key_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let idx = match ctx.get_field(this, KEY_FIELD_ALGO) {
        Value::Int(i) => i,
        _ => -1,
    };
    let s = ctx.create_string(algo_name(idx));
    Ok(Some(Value::Object(Some(s))))
}

fn key_get_encoded(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let der = if ctx.object_num_fields(this) > KEY_FIELD_DER {
        match ctx.get_field(this, KEY_FIELD_DER) {
            Value::Object(Some(arr)) => read_byte_array(ctx, arr),
            _ => Vec::new(),
        }
    } else {
        // `keystore::engine_get_key` deliberately uses a compact four-slot
        // PrivateKey proxy: slot 3 identifies the staged entry, so there is no
        // in-object DER at slot 4. Resolve that handle instead of treating the
        // key as the five-slot KeyFactory synthetic layout.
        crate::keystore::private_key_der_from_proxy(ctx, this).unwrap_or_default()
    };
    let arr = alloc_byte_array(ctx, &der);
    Ok(Some(Value::Object(Some(arr))))
}

fn pubkey_get_format(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let s = ctx.create_string("X.509");
    Ok(Some(Value::Object(Some(s))))
}

fn privkey_get_format(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let s = ctx.create_string("PKCS#8");
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// <clinit> shims for JCA classes the bytecode walks before our intercepts
// fire.
// ---------------------------------------------------------------------------

fn clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(r: &mut NativeMethodRegistry) {
    // Real-JCA bring-up: skip these synthetic short-circuit shims so
    // KeyPairGenerator/KeyFactory.getInstance fall through to the real
    // JDK 25 + BouncyCastle provider bytecode (yields concrete BCEC/BCRSA
    // keys instead of bare-interface `java/security/PrivateKey` synthetics).
    if crate::real_jca_mode() {
        return;
    }
    let kpg = "java/security/KeyPairGenerator";
    r.register(
        kpg,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyPairGenerator;",
        kpg_get_instance,
    );
    r.register(
        kpg,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/KeyPairGenerator;",
        kpg_get_instance,
    );
    r.register(
        kpg,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/KeyPairGenerator;",
        kpg_get_instance,
    );
    r.register(kpg, "initialize", "(I)V", kpg_initialize_int);
    r.register(
        kpg,
        "initialize",
        "(ILjava/security/SecureRandom;)V",
        kpg_initialize_int_random,
    );
    r.register(
        kpg,
        "initialize",
        "(Ljava/security/spec/AlgorithmParameterSpec;)V",
        kpg_initialize_spec,
    );
    r.register(
        kpg,
        "initialize",
        "(Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
        kpg_initialize_spec_random,
    );
    r.register(
        kpg,
        "generateKeyPair",
        "()Ljava/security/KeyPair;",
        kpg_generate_key_pair,
    );
    r.register(
        kpg,
        "genKeyPair",
        "()Ljava/security/KeyPair;",
        kpg_generate_key_pair,
    );
    r.register(
        kpg,
        "getAlgorithm",
        "()Ljava/lang/String;",
        kpg_get_algorithm,
    );
    // <clinit> shim — the JDK-25 KeyPairGenerator.<clinit> reads
    // `sun.security.util.Debug.getInstance("jca", "KeyPairGenerator")`
    // which we already shim, but defensively no-op the whole clinit so
    // any future field bring-up failure doesn't cascade.
    r.register(kpg, "<clinit>", "()V", clinit_noop);

    let kf = "java/security/KeyFactory";
    r.register(
        kf,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyFactory;",
        kf_get_instance,
    );
    r.register(
        kf,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/KeyFactory;",
        kf_get_instance,
    );
    r.register(
        kf,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/KeyFactory;",
        kf_get_instance,
    );
    r.register(
        kf,
        "generatePublic",
        "(Ljava/security/spec/KeySpec;)Ljava/security/PublicKey;",
        kf_generate_public,
    );
    r.register(
        kf,
        "generatePrivate",
        "(Ljava/security/spec/KeySpec;)Ljava/security/PrivateKey;",
        kf_generate_private,
    );
    r.register(kf, "getAlgorithm", "()Ljava/lang/String;", kf_get_algorithm);
    r.register(
        kf,
        "translateKey",
        "(Ljava/security/Key;)Ljava/security/Key;",
        kf_translate_key,
    );
    r.register(
        kf,
        "getKeySpec",
        "(Ljava/security/Key;Ljava/lang/Class;)Ljava/security/spec/KeySpec;",
        kf_get_key_spec,
    );
    r.register(kf, "<clinit>", "()V", clinit_noop);

    let kp = "java/security/KeyPair";
    r.register(
        kp,
        "getPublic",
        "()Ljava/security/PublicKey;",
        keypair_get_public,
    );
    r.register(
        kp,
        "getPrivate",
        "()Ljava/security/PrivateKey;",
        keypair_get_private,
    );

    // BouncyCastle's static key reconstructor. Its EC converter is unregistered
    // (EC$Mappings.configure is no-op'd), so the real BC getPublicKey returns
    // null for EC certs → X509CertificateObject.getPublicKey() null. Rebuild
    // EC/RSA keys from the SubjectPublicKeyInfo via the real KeyFactories.
    r.register(
        "org/bouncycastle/jce/provider/BouncyCastleProvider",
        "getPublicKey",
        "(Lorg/bouncycastle/asn1/x509/SubjectPublicKeyInfo;)Ljava/security/PublicKey;",
        bc_provider_get_public_key,
    );

    // Public/Private Key common accessors.  The synthetic `PublicKey` /
    // `PrivateKey` classes are interfaces in the JDK; we treat them as
    // concrete proxies here.  The real-JDK implementation classes are
    // `sun.security.provider.RSAPublicKey` etc., but `getInstance` /
    // `KeyFactory.generatePublic` return our synthetic, and the JDK code
    // dispatches on the interface — invokeinterface walks our synthetic
    // class's method table.  That works because the registry is keyed
    // by class name and our synthetic class is named
    // `java/security/PublicKey`.
    r.register(
        "java/security/PublicKey",
        "getAlgorithm",
        "()Ljava/lang/String;",
        key_get_algorithm,
    );
    r.register(
        "java/security/PublicKey",
        "getEncoded",
        "()[B",
        key_get_encoded,
    );
    r.register(
        "java/security/PublicKey",
        "getFormat",
        "()Ljava/lang/String;",
        pubkey_get_format,
    );
    r.register(
        "java/security/PrivateKey",
        "getAlgorithm",
        "()Ljava/lang/String;",
        key_get_algorithm,
    );
    r.register(
        "java/security/PrivateKey",
        "getEncoded",
        "()[B",
        key_get_encoded,
    );
    r.register(
        "java/security/PrivateKey",
        "getFormat",
        "()Ljava/lang/String;",
        privkey_get_format,
    );

    // <clinit> shim for sun.security.jca.GetInstance — the bytecode-side
    // helper that throws our NPE.  No-opping is safe because we never
    // dispatch into this class once getInstance() is intercepted.
    r.register("sun/security/jca/JCAUtil", "<clinit>", "()V", clinit_noop);

    // Signature also goes through `Signature.<clinit>` -> Debug; shim it
    // too so jca::signature can fire its overrides without the bytecode
    // running first.
    r.register("java/security/Signature", "<clinit>", "()V", clinit_noop);
    r.register(
        "java/security/MessageDigest",
        "<clinit>",
        "()V",
        clinit_noop,
    );

    // ECGenParameterSpec.<init>(String) — no-op except for stashing the
    // curve name so initialize(spec) can inspect it.  We intentionally
    // do NOT register getName etc. — the EC native ignores the curve
    // because we only support P-256.
    r.register(
        "java/security/spec/ECGenParameterSpec",
        "<init>",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let this = this_arg(args)?;
            if let Some(Value::Object(Some(s))) = args.get(1) {
                ctx.set_field(this, 0, Value::Object(Some(*s)));
            }
            Ok(None)
        },
    );
    r.register(
        "java/security/spec/ECGenParameterSpec",
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = this_arg(args)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

    #[test]
    fn algo_idx_round_trip() {
        assert_eq!(algo_idx("RSA"), ALGO_RSA);
        assert_eq!(algo_idx("rsa"), ALGO_RSA);
        assert_eq!(algo_idx("EC"), ALGO_EC);
        assert_eq!(algo_idx("ECDSA"), ALGO_EC);
        assert_eq!(algo_idx("Ed25519"), ALGO_ED25519);
        assert_eq!(algo_idx("Ed448"), ALGO_ED448);
        assert_eq!(algo_idx("RSASSA-PSS"), ALGO_RSA);
        assert_eq!(algo_idx("DSA"), ALGO_DSA);
        assert_eq!(algo_idx("DSS"), ALGO_DSA);
        assert_eq!(algo_idx("Garbage"), -1);
    }

    #[test]
    fn algo_name_round_trip() {
        assert_eq!(algo_name(ALGO_RSA), "RSA");
        assert_eq!(algo_name(ALGO_EC), "EC");
        assert_eq!(algo_name(ALGO_ED25519), "Ed25519");
        assert_eq!(algo_name(ALGO_ED448), "Ed448");
        assert_eq!(algo_name(ALGO_DSA), "DSA");
        assert_eq!(algo_name(-1), "Unknown");
        assert_eq!(algo_name(ALGO_X25519), "X25519");
        assert_eq!(algo_name(ALGO_X448), "X448");
        assert_eq!(algo_name(ALGO_XDH_GENERIC), "XDH");
        assert_eq!(algo_name(ALGO_EDDSA_GENERIC), "EdDSA");
        assert_eq!(algo_name(ALGO_RSASSA_PSS), "RSASSA-PSS");
    }

    /// `KeyFactory`'s algorithm resolution deliberately diverges from the
    /// shared (KeyPairGenerator-facing) `algo_idx` for exactly the names
    /// whose real-JDK KeyFactory SPI differs — see `kf_algo_idx`'s doc
    /// comment. `PemPrivateKeyParser` (the Spring Boot SSL PEM bundle loader)
    /// is the concrete caller that needs every one of these.
    #[test]
    fn kf_algo_idx_diverges_from_shared_algo_idx_where_needed() {
        assert_eq!(kf_algo_idx("RSASSA-PSS"), ALGO_RSASSA_PSS);
        assert_ne!(kf_algo_idx("RSASSA-PSS"), algo_idx("RSASSA-PSS"));
        assert_eq!(kf_algo_idx("XDH"), ALGO_XDH_GENERIC);
        assert_eq!(kf_algo_idx("EdDSA"), ALGO_EDDSA_GENERIC);
        assert_ne!(kf_algo_idx("EdDSA"), algo_idx("EdDSA"));
        assert_eq!(kf_algo_idx("X25519"), ALGO_X25519);
        assert_eq!(kf_algo_idx("X448"), ALGO_X448);
        assert_eq!(kf_algo_idx("Ed25519"), ALGO_ED25519);
        assert_eq!(kf_algo_idx("Ed448"), ALGO_ED448);
        // Everything else still falls through to the shared table unchanged.
        assert_eq!(kf_algo_idx("RSA"), ALGO_RSA);
        assert_eq!(kf_algo_idx("EC"), ALGO_EC);
        assert_eq!(kf_algo_idx("Garbage"), -1);
    }

    #[test]
    fn xdh_keyfactory_spi_classes_are_curve_specific() {
        assert_eq!(
            xdh_keyfactory_spi_class(ALGO_X25519),
            Some("sun/security/ec/XDHKeyFactory$X25519")
        );
        assert_eq!(
            xdh_keyfactory_spi_class(ALGO_X448),
            Some("sun/security/ec/XDHKeyFactory$X448")
        );
        assert_eq!(xdh_keyfactory_spi_class(ALGO_EC), None);
        assert_eq!(xdh_keyfactory_spi_class(ALGO_XDH_GENERIC), None);
    }

    /// The generic-name curve sniff: given a minimal PKCS#8 `PrivateKeyInfo`
    /// carrying each OID, `resolve_curve_algo` must recover the concrete
    /// curve, and pass concrete algos through untouched.
    #[test]
    fn resolve_curve_algo_sniffs_oid_from_pkcs8_spec() {
        fn pkcs8_stub(oid: &[u8]) -> Vec<u8> {
            // SEQUENCE { INTEGER 0, SEQUENCE { OID }, OCTET STRING { 0x04 00 } }
            let mut algid = vec![0x06, oid.len() as u8];
            algid.extend_from_slice(oid);
            let mut algid_seq = vec![0x30, algid.len() as u8];
            algid_seq.extend_from_slice(&algid);
            let octet = [0x04, 0x00];
            let mut inner = vec![0x02, 0x01, 0x00];
            inner.extend_from_slice(&algid_seq);
            inner.extend_from_slice(&octet);
            let mut out = vec![0x30, inner.len() as u8];
            out.extend_from_slice(&inner);
            out
        }
        for (oid, expected) in [
            (OID_X25519, ALGO_X25519),
            (OID_X448, ALGO_X448),
            (OID_ED25519, ALGO_ED25519),
            (OID_ED448, ALGO_ED448),
        ] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let der = pkcs8_stub(oid);
            let arr = alloc_byte_array(&mut ctx, &der);
            let spec =
                try_alloc_concurrent_synthetic(&mut ctx, "java/security/spec/PKCS8EncodedKeySpec", 1).unwrap();
            ctx.set_field(spec, 0, Value::Object(Some(arr)));
            let generic = if expected == ALGO_X25519 || expected == ALGO_X448 {
                ALGO_XDH_GENERIC
            } else {
                ALGO_EDDSA_GENERIC
            };
            assert_eq!(
                resolve_curve_algo(&mut ctx, generic, spec, true),
                expected,
                "OID {oid:02x?} should resolve to algo {expected}"
            );
        }
        // Concrete algos pass through unchanged regardless of the spec.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let arr = alloc_byte_array(&mut ctx, &[]);
        let spec =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/spec/PKCS8EncodedKeySpec", 1).unwrap();
        ctx.set_field(spec, 0, Value::Object(Some(arr)));
        assert_eq!(
            resolve_curve_algo(&mut ctx, ALGO_ED25519, spec, true),
            ALGO_ED25519
        );
        assert_eq!(resolve_curve_algo(&mut ctx, ALGO_RSA, spec, true), ALGO_RSA);
    }

    #[test]
    fn unknown_keyfactory_algorithm_throws_from_get_instance() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let name = ctx.create_string("1.2.840.113549.0.8456");
        let err = kf_get_instance(&mut ctx, &[Value::Object(Some(name))])
            .expect_err("unknown KeyFactory algorithm must not yield a synthetic factory");
        assert!(matches!(err, MethodCallFailed::ExceptionThrown(_)));
    }

    #[test]
    fn compact_keystore_private_key_get_encoded_uses_registry_der() {
        let alias = "key-factory-four-slot-private-key";
        let der = b"test-pkcs8-der".to_vec();
        let store = crate::keystore::LoadedKeyStore {
            entries: [(
                alias.to_string(),
                crate::keystore::KeyStoreEntry {
                    alias: alias.to_string(),
                    creation_time_ms: 0,
                    kind: crate::keystore::EntryKind::PrivateKey {
                        key_der: der.clone(),
                        chain: Vec::new(),
                    },
                },
            )]
            .into_iter()
            .collect(),
        };
        let store_id = crate::keystore::keystore_register(store);
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let key = try_alloc_concurrent_synthetic(&mut ctx, "java/security/PrivateKey", 4).unwrap();
        let alias_hash = alias.as_bytes().iter().fold(0x811c_9dc5_u32, |hash, byte| {
            (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
        });
        let composite = ((i64::from(store_id) & 0xffff_ffff) << 32) | i64::from(alias_hash);
        ctx.set_field(key, KEY_FIELD_KEYID, Value::Long(composite));

        let encoded = key_get_encoded(&mut ctx, &[Value::Object(Some(key))])
            .expect("compact key getEncoded must succeed")
            .expect("compact key getEncoded must return a byte array");
        let Value::Object(Some(array)) = encoded else {
            panic!("expected byte[] from compact key getEncoded");
        };
        assert_eq!(read_byte_array(&mut ctx, array), der);
    }

    #[test]
    fn keypairgenerator_preserves_requested_algorithm_name() {
        for requested in ["RSASSA-PSS", "Ed448", "Totally-Bogus"] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let name = ctx.create_string(requested);
            let kpg = kpg_get_instance(&mut ctx, &[Value::Object(Some(name))])
                .unwrap()
                .unwrap();
            let kpg_ref = match kpg {
                Value::Object(Some(o)) => o,
                other => panic!("expected KeyPairGenerator, got {other:?}"),
            };
            let result = kpg_get_algorithm(&mut ctx, &[Value::Object(Some(kpg_ref))])
                .unwrap()
                .unwrap();
            let actual = match result {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap(),
                other => panic!("expected algorithm String, got {other:?}"),
            };
            assert_eq!(actual, requested);
        }
    }

    #[test]
    fn bc_provider_detection_includes_bcfips() {
        assert!(is_bc_provider("BC"));
        assert!(is_bc_provider("BCFIPS"));
        assert!(is_bc_provider("BouncyCastle Security Provider"));
        assert!(is_bc_fips_provider("BCFIPS"));
        assert!(!is_bc_fips_provider("BC"));
        assert!(!is_bc_provider("SunEC"));
    }

    /// No-synthetic-stubs policy: a `KeyPairGenerator` for an algorithm we
    /// recognise but cannot implement (ML-KEM, ML-DSA, X25519) — or an outright
    /// unknown name — must throw from `generateKeyPair`, never return a
    /// `KeyPair` with empty key material. The previous fallback minted an
    /// empty-DER / `key_id == 0` key, presenting failed keygen as success.
    #[test]
    fn unimplemented_algorithm_keygen_throws_not_empty_key() {
        for algo in [
            "ML-KEM-512",
            "ML-KEM-768",
            "ML-DSA-44",
            "ML-DSA-65",
            "X25519",
            "Totally-Bogus",
        ] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let name = ctx.create_string(algo);
            let kpg = kpg_get_instance(&mut ctx, &[Value::Object(Some(name))])
                .expect("getInstance should not fail")
                .expect("getInstance should return a KeyPairGenerator");
            let err = kpg_generate_key_pair(&mut ctx, &[kpg]).expect_err(&format!(
                "{algo} generateKeyPair must throw, not silently return an empty key"
            ));
            // Real-JDK mode constructs a genuine NoSuchAlgorithmException; the
            // mock's `new_object_initialized` yields a Throwable object, so we
            // get `ExceptionThrown` (the catchable path) rather than the
            // `SecurityException` defensive fallback.
            match err {
                MethodCallFailed::ExceptionThrown(_) => {}
                other => panic!(
                    "{algo}: expected ExceptionThrown(NoSuchAlgorithmException), got {other:?}"
                ),
            }
        }
    }

    /// Guard against the fail-closed change accidentally catching RSA: RSA must
    /// still produce a `KeyPair` backed by real DER + a non-zero `key_id`. Uses
    /// a small key size to keep the software keygen fast.
    #[test]
    fn rsa_keygen_still_returns_real_key_material() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let name = ctx.create_string("RSA");
        let kpg = kpg_get_instance(&mut ctx, &[Value::Object(Some(name))])
            .unwrap()
            .unwrap();
        let kpg_ref = match kpg {
            Value::Object(Some(o)) => o,
            other => panic!("expected KeyPairGenerator, got {other:?}"),
        };
        // initialize(512) — exercises the real keygen path without the cost of
        // a 2048-bit prime search.
        kpg_initialize_int(&mut ctx, &[Value::Object(Some(kpg_ref)), Value::Int(512)]).unwrap();
        let kp = match kpg_generate_key_pair(&mut ctx, &[Value::Object(Some(kpg_ref))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected KeyPair object, got {other:?}"),
        };
        let pubk = match ctx.get_field(kp, 0) {
            Value::Object(Some(o)) => o,
            other => panic!("expected public key, got {other:?}"),
        };
        assert!(
            matches!(ctx.get_field(pubk, KEY_FIELD_KEYID), Value::Long(id) if id != 0),
            "RSA public key must carry a real key_id"
        );
        match ctx.get_field(pubk, KEY_FIELD_DER) {
            Value::Object(Some(der)) => {
                assert!(
                    ctx.array_length(der) > 0,
                    "RSA public DER must be non-empty"
                )
            }
            other => panic!("expected DER byte[], got {other:?}"),
        }
    }

    /// Build a `KeyFactory` synthetic for `algo` and call `generatePublic` /
    /// `generatePrivate` with a KeySpec whose encoded byte[] (field 0) is `der`.
    fn kf_call(
        ctx: &mut crate::test_utils::MockNativeContext,
        algo: &str,
        method: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
        der: &[u8],
    ) -> Result<Option<Value>, MethodCallFailed> {
        let name = ctx.create_string(algo);
        // `?`, not `.unwrap()`: `kf_get_instance` now rejects an unrecognised
        // algorithm name up front (`kf_get_instance`'s own doc comment), so
        // `"Totally-Bogus"` fails here rather than at `kf_generate_public`/
        // `kf_generate_private` below — still satisfies every caller's
        // `expect_err(...)`, since they only care that *some*
        // `MethodCallFailed` comes back, not which stage produced it.
        let kf = match kf_get_instance(ctx, &[Value::Object(Some(name))])? {
            Some(v) => v,
            None => panic!("kf_get_instance returned no KeyFactory"),
        };
        let kf_ref = match kf {
            Value::Object(Some(o)) => o,
            other => panic!("expected KeyFactory, got {other:?}"),
        };
        let spec = try_alloc_concurrent_synthetic(ctx, "java/security/spec/X509EncodedKeySpec", 1)?;
        let der_arr = alloc_byte_array(ctx, der);
        ctx.set_field(spec, 0, Value::Object(Some(der_arr)));
        method(
            ctx,
            &[Value::Object(Some(kf_ref)), Value::Object(Some(spec))],
        )
    }

    /// No-synthetic-stubs policy on the KeyFactory import path: when no usable
    /// key can be produced — an unimplemented algorithm, or an RSA/EC spec that
    /// fails to parse — generatePublic/generatePrivate must throw
    /// `InvalidKeySpecException`, never return a `key_id == 0` key that silently
    /// fails every later verify/sign.
    #[test]
    fn keyfactory_unproducible_key_throws_not_dead_key() {
        // generatePublic: unimplemented algorithms.
        for algo in [
            "ML-KEM-512",
            "ML-DSA-65",
            "X25519",
            "Ed25519",
            "Totally-Bogus",
        ] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let err = kf_call(&mut ctx, algo, kf_generate_public, &[1, 2, 3]).expect_err(&format!(
                "{algo} generatePublic must throw, not return a dead key"
            ));
            assert!(
                matches!(err, MethodCallFailed::ExceptionThrown(_)),
                "{algo} generatePublic: expected ExceptionThrown(InvalidKeySpecException), got {err:?}"
            );
        }
        // generatePublic: RSA with an unparseable spec.
        {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let err = kf_call(
                &mut ctx,
                "RSA",
                kf_generate_public,
                &[0xDE, 0xAD, 0xBE, 0xEF],
            )
            .expect_err("RSA generatePublic with garbage DER must throw");
            assert!(
                matches!(err, MethodCallFailed::ExceptionThrown(_)),
                "got {err:?}"
            );
        }
        // generatePrivate: no private-key importer exists for RSA (or anything
        // but the real-SunEC EC path) → must throw.
        for algo in ["RSA", "ML-DSA-65"] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let err = kf_call(&mut ctx, algo, kf_generate_private, &[1, 2, 3]).expect_err(
                &format!("{algo} generatePrivate must throw, not return a dead key"),
            );
            assert!(
                matches!(err, MethodCallFailed::ExceptionThrown(_)),
                "{algo} generatePrivate: expected ExceptionThrown(InvalidKeySpecException), got {err:?}"
            );
        }
    }

    #[test]
    fn eddsa_keyfactory_spi_classes_are_curve_specific() {
        assert_eq!(
            eddsa_keyfactory_spi_class(ALGO_ED25519),
            Some("sun/security/ec/ed/EdDSAKeyFactory$Ed25519")
        );
        assert_eq!(
            eddsa_keyfactory_spi_class(ALGO_ED448),
            Some("sun/security/ec/ed/EdDSAKeyFactory$Ed448")
        );
        assert_eq!(eddsa_keyfactory_spi_class(ALGO_EC), None);
    }

    /// Guard the real RSA public-key import path: a valid SubjectPublicKeyInfo
    /// DER must still round-trip to a key backed by a real `key_id` — the
    /// fail-closed change must not catch the parseable case.
    #[test]
    fn keyfactory_rsa_public_import_still_returns_real_key() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let (pk, _sk) = crypto_impl::Rsa::generate_keypair(512);
        let der = crypto_impl::Rsa::public_key_to_der(&pk);
        let pub_obj = match kf_call(&mut ctx, "RSA", kf_generate_public, &der)
            .expect("valid RSA spec must not throw")
            .expect("generatePublic must return a key")
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected PublicKey, got {other:?}"),
        };
        assert!(
            matches!(ctx.get_field(pub_obj, KEY_FIELD_KEYID), Value::Long(id) if id != 0),
            "imported RSA public key must carry a real key_id"
        );
    }
}
