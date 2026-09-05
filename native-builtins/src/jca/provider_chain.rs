// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.1 — `java.security.Security` provider list + `java.security.Provider`
//! accessors. Available in real-JDK mode (synthetic-jdk wires the same
//! surface through `phases_early::register_phase53_security`, so when both
//! are wired the latter idempotently overwrites with identical callbacks).
//!
//! ## Provider chain
//!
//! Seeded from the JDK 25 default list (`java.security` properties file
//! shipped with the reference HotSpot 25.0.1 install). The chain is a
//! single global `Mutex<Vec<(name, version_str)>>` so `addProvider /
//! insertProviderAt / removeProvider` mutations survive across native
//! callbacks; we hand out fresh `Provider` synthetics on every read so
//! we never hold a heap `ObjectRef` past the originating callback.
//!
//! ## Provider object layout
//!
//! Allocated through `try_alloc_concurrent_synthetic("java/security/Provider", N)?`,
//! which calls `ensure_class_initialized` and uses the *larger* of the
//! requested field count and `class_num_total_fields` (so the real JDK
//! field map — `name`, `info`, `version`, `versionStr`, … — is fully
//! sized). We override the `getName` / `getVersion` / `getVersionStr` /
//! `toString` / `getInfo` accessors so the actual slot indices used here
//! are an internal detail — the real JDK fields are never read directly
//! by our natives:
//!
//! | Slot | Native getter             | Storage           |
//! |------|---------------------------|-------------------|
//! |  0   | `getName()`               | String            |
//! |  1   | `getVersionStr()` source  | Double            |
//! |  2   | `getInfo()` source        | String (synthetic)|
//!
//! Slot 1 stores the *numeric* version (e.g. 25.0) so the existing
//! `getVersion()` D-typed return path keeps working; `getVersionStr()`
//! formats it as `"<int(version)>"` to match JDK 25's HotSpot output
//! (`"25"` not `"25.0"`).
//!
//! **That table describes the SYNTHETIC layout only, and since G43-1 it is
//! only written when the receiver actually has it.** On a real image
//! `java.security.Provider extends java.util.Properties extends
//! java.util.Hashtable`, so slots 0/1/2 are the inherited `table` /
//! `count` / `threshold` and writing this table onto one corrupts a live
//! hash map. `make_provider` decides per instance, by reading back its own
//! `set_field_by_name("name", …)` — see `provider_has_named_layout`.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, VmError};
use cratonvm_types::{ObjectRef, Value};

use rustc_hash::FxHashMap;

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Provider chain — process-wide mutable list mirroring HotSpot's default
// JDK 25 ordering. The list is consulted by `Security.getProviders` and
// `Security.getProvider(name)`; mutations from `addProvider /
// insertProviderAt / removeProvider` persist for the lifetime of the VM.
// ---------------------------------------------------------------------------

// C19: each seed-list entry carries a `coverage` description so callers
// can tell, via `Provider.getInfo()`, what the provider actually backs.
// "Unbacked" providers (no Service map registered) advertise:
//   "coverage: none — placeholder provider for legacy app compatibility,
//    no Service map registered"
// so downstream `Provider.getService(...)` returning null isn't a mystery
// when the diagnostic string is inspected.  Backed providers describe
// the actual algorithm set wired through the rest of the `jca/` tree
// (see `message_digest.rs`, `signature.rs`, `cipher.rs`).
//
// User-added providers (via `Security.addProvider` / `insertProviderAt`)
// get `USER_PROVIDER_COVERAGE` since we don't introspect their Service
// map — the caller furnished the Provider instance.
const COVERAGE_UNBACKED: &str =
    "coverage: none — placeholder provider for legacy app compatibility, no Service map registered";
const COVERAGE_SUN: &str =
    "coverage: MessageDigest{MD5,SHA-1,SHA-224,SHA-256,SHA-384,SHA-512,SHA-512/224,SHA-512/256,\
     SHA3-224,SHA3-256,SHA3-384,SHA3-512}, SecureRandom";
const COVERAGE_SUN_RSA_SIGN: &str = "coverage: KeyPairGenerator{RSA}, KeyFactory{RSA}, \
     Signature{SHA1withRSA,SHA256withRSA,SHA384withRSA,SHA512withRSA}";
const COVERAGE_SUN_JCE: &str =
    "coverage: Cipher{AES,ChaCha20,RSA,PBE}, KeyFactory{ML-KEM} via direct native JCA routes";
const COVERAGE_SUN_EC: &str =
    "coverage: KeyFactory{EC,Ed25519,Ed448,X25519,X448}, Signature{ECDSA,EdDSA}";
const COVERAGE_SUN_JSSE: &str = "coverage: Signature{MD5andSHA1withRSA}";
const COVERAGE_SUN_MSCAPI: &str =
    "coverage: Cipher{RSA}, SecureRandom{Windows-PRNG}, Signature{RSA,ECDSA}";
const USER_PROVIDER_COVERAGE: &str =
    "coverage: user-registered Provider — Service map (if any) supplied by caller";

fn provider_chain() -> &'static parking_lot::Mutex<Vec<(String, f64, &'static str)>> {
    use std::sync::OnceLock;
    static CHAIN: OnceLock<parking_lot::Mutex<Vec<(String, f64, &'static str)>>> = OnceLock::new();
    CHAIN.get_or_init(|| {
        // Seed list — order, names, and version match the JDK 25.0.1
        // reference HotSpot output (captured via `Security.getProviders()`
        // on a stock install). Version 25.0 — formatted as "25" by
        // `getVersionStr()` to match HotSpot's `Provider.versionStr`.
        //
        // C19: third tuple field is the coverage disclosure string,
        // surfaced through `Provider.getInfo()` and consulted by the
        // `getService` debug-log path so an unbacked-provider lookup
        // can be diagnosed without spelunking the source.
        //
        // THE LIST IS PER-PLATFORM. `SunMSCAPI` wraps the Windows CryptoAPI
        // and ships only in the Windows JDK: `java.security` registers it
        // from a `#ifdef windows` block, and `sun.security.mscapi.SunMSCAPI`
        // is not present in a Linux or macOS image at all. The capture above
        // was evidently taken on Windows and the name was seeded
        // unconditionally, so `Security.getProviders()` on Linux answered a
        // thirteen-element list with `SunMSCAPI` at index 11 where HotSpot 25
        // on the same machine answers twelve without it — and
        // `Security.getProvider("SunMSCAPI")` handed back a live Provider
        // advertising 16 services for a class that raises
        // ClassNotFoundException. Any code that iterates the chain in order,
        // or that treats "the provider exists" as "the platform supports it",
        // sees a provider that cannot service anything.
        let mut seed = vec![
            ("SUN".to_string(), 25.0, COVERAGE_SUN),
            ("SunRsaSign".to_string(), 25.0, COVERAGE_SUN_RSA_SIGN),
            ("SunEC".to_string(), 25.0, COVERAGE_SUN_EC),
            ("SunJSSE".to_string(), 25.0, COVERAGE_SUN_JSSE),
            ("SunJCE".to_string(), 25.0, COVERAGE_SUN_JCE),
            ("SunJGSS".to_string(), 25.0, COVERAGE_UNBACKED),
            ("SunSASL".to_string(), 25.0, COVERAGE_UNBACKED),
            ("XMLDSig".to_string(), 25.0, COVERAGE_UNBACKED),
            ("SunPCSC".to_string(), 25.0, COVERAGE_UNBACKED),
            ("JdkLDAP".to_string(), 25.0, COVERAGE_UNBACKED),
            ("JdkSASL".to_string(), 25.0, COVERAGE_UNBACKED),
        ];
        if cfg!(target_os = "windows") {
            seed.push(("SunMSCAPI".to_string(), 25.0, COVERAGE_SUN_MSCAPI));
        }
        seed.push(("SunPKCS11".to_string(), 25.0, COVERAGE_UNBACKED));
        parking_lot::Mutex::new(seed)
    })
}

fn snapshot() -> Vec<(String, f64, &'static str)> {
    provider_chain().lock().clone()
}

/// The installed providers' names, in chain (preference) order.
///
/// Exists so an engine whose own `getInstance` interception cannot serve a name
/// can do what `ProviderList.getService` does — ask each installed provider in
/// turn — instead of refusing outright. `getinstance_get_service_search` already
/// walked this list; nothing outside this module could.
pub(crate) fn chain_provider_names() -> Vec<String> {
    snapshot().into_iter().map(|(name, _, _)| name).collect()
}

pub(crate) fn find(name: &str) -> Option<(f64, &'static str)> {
    provider_chain()
        .lock()
        .iter()
        .find(|(n, _, _)| n == name)
        .map(|(_, v, c)| (*v, *c))
}

/// True if the named provider is in the seed list with coverage
/// marked as unbacked. Used by `getService` to log a hint when a
/// lookup hits a placeholder provider (which always returns null
/// because no Service entries are ever registered to its name).
///
/// Compares coverage strings by value rather than pointer identity —
/// `const &str` items in Rust are not guaranteed to share an address
/// across uses, so `std::ptr::eq` would falsely report mismatches in
/// release builds where the compiler may duplicate the data.
fn is_unbacked_provider(name: &str) -> bool {
    provider_chain()
        .lock()
        .iter()
        .any(|(n, _, c)| n == name && *c == COVERAGE_UNBACKED)
}

fn add(name: String, ver: f64) -> i32 {
    let mut list = provider_chain().lock();
    if let Some(idx) = list.iter().position(|(n, _, _)| *n == name) {
        list[idx].1 = ver;
        return (idx + 1) as i32;
    }
    list.push((name, ver, USER_PROVIDER_COVERAGE));
    list.len() as i32
}

fn insert_at(name: String, ver: f64, pos: i32) -> i32 {
    let mut list = provider_chain().lock();
    if let Some(idx) = list.iter().position(|(n, _, _)| *n == name) {
        return (idx + 1) as i32;
    }
    let target = if pos < 1 {
        list.len()
    } else {
        ((pos - 1) as usize).min(list.len())
    };
    list.insert(target, (name, ver, USER_PROVIDER_COVERAGE));
    (target + 1) as i32
}

fn remove(name: &str) {
    let mut list = provider_chain().lock();
    if let Some(idx) = list.iter().position(|(n, _, _)| n == name) {
        list.remove(idx);
    }
}

/// Real `java.security.Provider` objects handed to `Security.addProvider` /
/// `insertProviderAt`, keyed by provider name -> a permanent GC root handle
/// (see `NativeContext::add_global_root`). Every OTHER read of a provider
/// (`Security.getProvider`, `getProviders`, `getService`'s internal
/// `make_provider` calls, …) hands out a *fresh synthetic* `Provider`
/// (module-top doc: "we never hold a heap `ObjectRef` past the originating
/// callback") — deliberately, since most callers only need `getName`/
/// `getVersion`/`getProperty`, which our synthetics answer correctly without
/// pinning the real object forever.
///
/// Some providers, though, carry private state no JCA-visible accessor
/// exposes: BouncyCastle-FIPS's `BouncyCastleFipsProvider` resolves
/// `getService()`/`Service.newInstance()` through its own private
/// `creatorMap` (an `EngineCreator` factory keyed by the same `className`
/// string it also `put()`s into the inherited legacy Hashtable — see
/// `try_engine_creator_instantiate`), never through the reflectable
/// `className` our `ServiceEntry`/synthetic-Service path assumes. Retaining
/// the real object here is what lets `build_jca_instance` reach that map.
fn real_provider_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<String, usize>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<String, usize>>> =
        OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Remember `prov` (a real `Provider` object, e.g. just passed to
/// `Security.addProvider`) as the real-object entry for `name`, replacing
/// (and releasing the global root of) any previous entry for the same name.
fn remember_real_provider(ctx: &mut dyn NativeContext, name: &str, prov: ObjectRef) {
    let handle = ctx.add_global_root(prov);
    if handle == 0 {
        return;
    }
    let old = real_provider_table()
        .lock()
        .insert(name.to_string(), handle);
    if let Some(old_handle) = old {
        ctx.remove_global_root(old_handle);
    }
}

/// Resolve the real `Provider` object registered for `name`, if any — see
/// `real_provider_table`.
fn resolve_real_provider(ctx: &mut dyn NativeContext, name: &str) -> Option<ObjectRef> {
    let handle = real_provider_table().lock().get(name).copied()?;
    ctx.resolve_global_root(handle)
}

// ---------------------------------------------------------------------------
// Provider synthetic — slot layout documented at module top.
// ---------------------------------------------------------------------------

/// Did the `set_field_by_name("versionStr", …)` write in [`make_provider`]
/// actually land — i.e. does this `Provider` have the REAL JDK layout rather
/// than CratonVM's synthetic one?
///
/// Exactly the read-back predicate `service_has_named_layout` uses for
/// `Provider$Service`, for exactly the same reason: it answers the question
/// the caller actually has ("did my named write take?") in both modes, with no
/// new `NativeContext` surface, and it is per-instance where a registration is
/// global.
///
/// `versionStr` and not `name`, for two reasons that happen to agree.
/// Substantively: `versionStr` is declared by `java.security.Provider` itself
/// and by nothing above it, so a receiver that satisfies it has Provider's own
/// layout, not merely some superclass's. Mechanically: the unit-test mock
/// resolves the bare name `name` through a class-blind mirror table
/// (`test_utils::mock_jdk_field_slot`), so a `name`-based predicate would
/// answer "real layout" under every test in this file and the synthetic arm
/// would be untestable — the DIVERGENCE hazard `MockNativeContext::
/// get_field_by_name` documents, met head-on rather than papered over.
///
/// G43-1 — why the answer matters. `java.security.Provider extends
/// java.util.Properties extends java.util.Hashtable extends java.util.Dictionary`,
/// so on a real image slots 0/1/2 of a `Provider` are not Provider's own
/// fields at all; they are `Hashtable.table:[Ljava/util/Hashtable$Entry;`,
/// `Hashtable.count:I` and `Hashtable.threshold:I`. (The comment on the
/// accessors below used to say slot 0+1 was `serialVersionUID` and slot 2 was
/// `debug`. Both of those are STATIC — `javap -p java.security.Provider`, JDK
/// 25.0.3+9 — so they occupy no instance slot, and the real occupants are the
/// inherited `Hashtable` ones.) The legacy mirror therefore wrote a `String`
/// over the hash table, a `Double` over `count`, and a heap ADDRESS over
/// `threshold` — the last of which is the `pointer-into-primitive` coercion
/// species `G30-1` §4.1 declared could not occur, MEASURED firing twice per
/// `RCrypto` run at `make_provider`.
fn provider_has_named_layout(
    ctx: &mut dyn NativeContext,
    provider: ObjectRef,
    version_str_obj: ObjectRef,
) -> bool {
    matches!(
        ctx.get_field_by_name(provider, "versionStr"),
        Value::Object(Some(got)) if got == version_str_obj
    )
}

/// Materialise a fresh `java.security.Provider` synthetic with name +
/// numeric version. Used by every read-side path
/// (`getProviders`, `getProvider`); we never cache `ObjectRef` values
/// across callbacks so the heap is free to GC the previous instance.
pub(crate) fn make_provider(
    ctx: &mut dyn NativeContext,
    name: &str,
    version: f64,
    coverage: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // `alloc_concurrent_synthetic` upsizes to the real-JDK field count
    // (Provider has > 10 instance fields counting inherited Properties
    // slots), so writing to slots 0/1/2 stays in bounds even when our
    // intended layout is smaller than the real one.
    //
    // WP6.1 layout fix: writing by raw slot index (0/1/2) collides with
    // inherited Hashtable fields (`table`/`count`/`threshold`/...) when
    // the real-JDK Provider class is loaded — slot 1 is `Hashtable.count`
    // (an `int`), so storing a `Double(25.0)` there reads back as
    // `Int(0)` and `getVersionStr()` prints "0".  Use `set_field_by_name`
    // so the slot is resolved against the actual class layout (`name` /
    // `version` / `versionStr` / `info` declared on Provider itself).
    // Synthetic-mode allocations (where the class isn't loaded and
    // `set_field_by_name` is a no-op) still fall back to the slot-based
    // path below so existing fixtures keep working.
    //
    // C19: `info` carries the cratonvm coverage disclosure so
    // `Provider.getInfo()` callers (logs, BC's `isFipsMode()` audit,
    // diagnostic dumps) can see whether the named provider actually
    // backs any algorithm — particularly important for unbacked
    // entries like SunPKCS11 / SunMSCAPI / SunPCSC that look real but
    // never resolve a Service.
    let p = try_alloc_concurrent_synthetic(ctx, "java/security/Provider", 8)?;
    let n = ctx.create_string(name);
    let info_str = format!("{} security provider (cratonvm) — {}", name, coverage);
    let info = ctx.create_string(&info_str);
    let ver_str_text = if version.fract() == 0.0 {
        format!("{}", version as i64)
    } else {
        let s = format!("{version}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    let ver_str = ctx.create_string(&ver_str_text);

    // Real-JDK path — resolve by name.
    ctx.set_field_by_name(p, "name", Value::Object(Some(n)));
    ctx.set_field_by_name(p, "version", Value::Double(version));
    ctx.set_field_by_name(p, "versionStr", Value::Object(Some(ver_str)));
    ctx.set_field_by_name(p, "info", Value::Object(Some(info)));

    // The real `java.security.Provider` constructor never runs for our
    // synthetic instances, so the inherited `initialized` boolean stays
    // false.  Any caller that reaches a real `Provider` method guarded by
    // `checkInitialized()` — `keys()`, `entrySet()`, `elements()`,
    // `getService()`, and crucially `Security.getAlgorithms(type)` which
    // iterates `provider.keys()` — then throws a bare `IllegalStateException`.
    // Tomcat's `SessionIdGeneratorBase.<clinit>` calls
    // `Security.getAlgorithms("SecureRandom")`, so the ISE is wrapped in an
    // `ExceptionInInitializerError` and the class is poisoned
    // (`NoClassDefFoundError` on every later use) — breaking session id
    // generation across the whole server, i.e. nearly every Catalina/Coyote
    // integration test.  Mark the provider initialized so those guards pass.
    // The backing Hashtable is empty (count == 0), so `keys()` returns an
    // empty enumeration and `getAlgorithms` yields an empty set rather than
    // throwing — graceful degradation (callers fall back to platform
    // defaults) instead of a hard failure.
    ctx.set_field_by_name(p, "initialized", Value::Int(1));

    // Synthetic fallback — populate the legacy slots 0/1/2 so the readers that
    // have not migrated to the real-JDK accessors still see consistent state:
    // `provider_get_name` (slot 0), `provider_get_version` /
    // `provider_get_version_str` / `provider_to_string` (slot 1) and
    // `provider_get_info` (slot 2) in this file, plus the `getName` / `getVersion`
    // Bridges `phases_early::register_phase53_security` registers, which read
    // slots 0 and 1.
    //
    // G43-1 — ONLY in synthetic mode, and the guard is the whole change.
    //
    // The two modes genuinely need different answers, and the trap named in
    // the previous lane's note is real: "correct the indices to the real JDK
    // layout" is not available, because there is nothing to correct them TO.
    // Slots 0/1/2 on a real `Provider` are inherited `Hashtable` state
    // (`table` / `count` / `threshold` — see `provider_has_named_layout`),
    // and Provider's own `name` / `version` / `info` already have the four
    // `set_field_by_name` writes above. Relocating the mirror to the real
    // `name`/`version`/`info` slots would just repeat those writes; pointing
    // it anywhere else corrupts a live `Hashtable`. The synthetic layout, in
    // turn, HAS no `Hashtable` and its readers are the slot-indexed ones. So
    // the correct value at slot 2 in synthetic mode and the correct value at
    // slot 2 in real-JDK mode are different values in different fields, and
    // the only reconciliation is to write the mirror only where it is the
    // truth.
    //
    // Skipping it in real-JDK mode is observationally inert on the read side:
    // every reader in this file consults `get_field_by_name` FIRST and only
    // falls through to the slot when the named read comes back absent, and the
    // named reads are exactly the writes we just made. `register_phase53_security`
    // is reached solely from `register_synthetic_overrides`, which is
    // `#[cfg(feature = "synthetic-jdk")]`, so its slot-indexed Bridges do not
    // exist in real-JDK mode at all — a 10,691-row `--jdk-only` registry dump
    // has zero rows for `java/security/Provider` from that registrar.
    //
    // What it removes is not inert: it stops publishing a heap address into
    // `Hashtable.threshold` and a `Double`'s raw bit pattern into
    // `Hashtable.count` on an object that IS a live `Hashtable` and that we
    // deliberately mark `initialized = 1` a few lines up so that real
    // `keys()` / `entrySet()` / `getAlgorithms` bytecode RUNS over it. Today
    // that survives only by luck — `Double(25.0).to_bits() as i32` is 0, so
    // `count == 0` and `Hashtable.getEnumeration` early-returns before it can
    // dereference the `String` sitting in `table`. Any provider version with a
    // fractional part (`1.8` → low 32 bits `0xCCCCCCCD`) makes `count` nonzero
    // and the next `keys()` walks a `String` as an `Entry[]`.
    if !provider_has_named_layout(ctx, p, ver_str) {
        ctx.set_field(p, 0, Value::Object(Some(n)));
        ctx.set_field(p, 1, Value::Double(version));
        ctx.set_field(p, 2, Value::Object(Some(info)));
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// Native callbacks
// ---------------------------------------------------------------------------

// WP6.5: Provider field accessors must be layout-aware. The synthetic
// Provider allocated by `make_provider` stores `name`/`version`/`info` in
// slots 0/1/2.  Real-JDK `java.security.Provider` puts something else there
// entirely, so reading slot 0 from a real-JDK Provider returned junk that
// appeared to callers as `null` and made every `Provider.getName()` /
// `getVersionStr()` call lie about the receiver, which in turn broke
// `BouncyCastleProvider.setup()` (it queries its own name from inside
// `loadServiceClass` to build cache keys).
//
// G43-1 CORRECTION. This comment used to say the real-JDK occupants were
// `serialVersionUID` (long, slot 0+1), `debug` (slot 2), `name` (slot 3),
// `info` (slot 4), `version` (double, slot 5+6), `versionStr` (slot 7).
// That is wrong twice over and the wrongness was load-bearing — it is why
// the legacy mirror in `make_provider` looked harmless. `javap -p
// java.security.Provider` on JDK 25.0.3+9: `serialVersionUID` and `debug`
// are both `static`, so they occupy no instance slot at all; and `Provider
// extends java.util.Properties extends java.util.Hashtable`, so the low
// slots belong to the SUPERCLASSES. The real occupants of 0/1/2 are
// `Hashtable.table:[Ljava/util/Hashtable$Entry;`, `Hashtable.count:I` and
// `Hashtable.threshold:I` — see `provider_has_named_layout`.
//
// Fix: prefer `get_field_by_name`, which resolves the slot from the
// receiver's actual class layout.  Fall back to slots 0/1/2 only when
// the receiver is a synthetic that doesn't have those fields registered
// (every `alloc_concurrent_synthetic` allocation should still work).
fn provider_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real JDK: read the `name` field. Synthetic: slot 0 carries the name
    // we wrote in make_provider. `get_field_by_name` returns
    // `Value::Object(None)` when the field is absent, so the slot-0
    // fallback only activates for synthetics.
    let by_name = ctx.get_field_by_name(this, "name");
    if matches!(&by_name, Value::Object(Some(_))) {
        return Ok(Some(by_name));
    }
    // G43-1: the slot fallback is only meaningful on a synthetic. On a real
    // Provider slot 0 is `Hashtable.table`, and an unwritten reference slot
    // reads back as `Int(0)` (field_read.rs's niche-0 note), so return the
    // slot only when it actually holds a reference — the descriptor here is
    // `()Ljava/lang/String;` and handing the interpreter an `Int` back is a
    // guaranteed type error at the call site.
    match ctx.get_field(this, 0) {
        v @ Value::Object(_) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(None))),
    }
}

fn provider_get_version(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real JDK declares `version` as a `double`; synthetics store it as
    // `Double` at slot 1.  Both readers funnel through the same coercion.
    let by_name = ctx.get_field_by_name(this, "version");
    let v = match by_name {
        Value::Double(d) => Some(d),
        Value::Int(i) => Some(i as f64),
        _ => None,
    };
    if let Some(d) = v {
        return Ok(Some(Value::Double(d)));
    }
    match ctx.get_field(this, 1) {
        Value::Double(d) => Ok(Some(Value::Double(d))),
        Value::Int(i) => Ok(Some(Value::Double(i as f64))),
        _ => Ok(Some(Value::Double(25.0))),
    }
}

/// `Provider.getVersionStr()` → JDK 25 returns `"25"`, not `"25.0"`.
/// Trailing `.0` is stripped to match HotSpot's `parseVersionStr` output.
fn provider_get_version_str(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real JDK has a dedicated `versionStr:String` field that is set by
    // the `(String, String, String)` constructor; honour it verbatim.
    let by_name = ctx.get_field_by_name(this, "versionStr");
    if let Value::Object(Some(s_obj)) = by_name {
        // Pass through the JDK-supplied string (may be "25", "25.0", "1.80.0", ...).
        return Ok(Some(Value::Object(Some(s_obj))));
    }

    // Synthetic fallback: format the numeric `version` slot ourselves.
    let ver = match ctx.get_field_by_name(this, "version") {
        Value::Double(d) => d,
        Value::Int(i) => i as f64,
        _ => match ctx.get_field(this, 1) {
            Value::Double(d) => d,
            Value::Int(i) => i as f64,
            _ => 25.0,
        },
    };
    let formatted = if ver.fract() == 0.0 {
        format!("{}", ver as i64)
    } else {
        // Format compactly: 25.5 not 25.5000000…
        let s = format!("{ver}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    let s = ctx.create_string(&formatted);
    Ok(Some(Value::Object(Some(s))))
}

fn provider_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Pull name + version through the same field-by-name path so real-JDK
    // Provider instances render correctly (the synthetic fallbacks below
    // only fire when nothing matches — keeps test fixtures using the
    // synthetic layout working).
    let name = {
        let by_name = ctx.get_field_by_name(this, "name");
        let raw = match by_name {
            Value::Object(Some(n)) => Some(n),
            _ => match ctx.get_field(this, 0) {
                Value::Object(Some(n)) => Some(n),
                _ => None,
            },
        };
        match raw {
            Some(n) => ctx.read_string(n).unwrap_or_else(|| "Provider".to_string()),
            None => "Provider".to_string(),
        }
    };
    // Prefer the JDK-supplied versionStr (matches HotSpot's `toString`).
    let ver_str = match ctx.get_field_by_name(this, "versionStr") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let ver_rendered = match ver_str {
        Some(s) => s,
        None => {
            let ver = match ctx.get_field_by_name(this, "version") {
                Value::Double(d) => d,
                Value::Int(i) => i as f64,
                _ => match ctx.get_field(this, 1) {
                    Value::Double(d) => d,
                    Value::Int(i) => i as f64,
                    _ => 25.0,
                },
            };
            if ver.fract() == 0.0 {
                format!("{}", ver as i64)
            } else {
                format!("{ver}")
            }
        }
    };
    let s = ctx.create_string(&format!("{name} version {ver_rendered}"));
    Ok(Some(Value::Object(Some(s))))
}

fn provider_get_info(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let by_name = ctx.get_field_by_name(this, "info");
    if matches!(&by_name, Value::Object(Some(_))) {
        return Ok(Some(by_name));
    }
    // G43-1: same rule as `provider_get_name`, and here it is not theoretical.
    // Slot 2 on a real `Provider` is `Hashtable.threshold:I`, so this fallback
    // could return an `Int` from a `()Ljava/lang/String;` native. It is also
    // the only reader of slot 2 anywhere in the tree — `register_phase53_security`'s
    // `getInfo` Bridge synthesises its string from slot 0 and never touches
    // slot 2 — which is what makes `make_provider`'s slot-2 write purely a
    // synthetic-mode obligation.
    match ctx.get_field(this, 2) {
        v @ Value::Object(_) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(None))),
    }
}

fn security_get_providers(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let chain = snapshot();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, chain.len());
    for (i, (name, ver, coverage)) in chain.iter().enumerate() {
        // Same identity rule as `security_get_provider`: an entry the
        // application registered is handed back as the object it registered,
        // not as a same-named stand-in.
        if let Some(real) = resolve_real_provider(ctx, name) {
            ctx.set_array_element(arr, i, Value::Object(Some(real)));
            continue;
        }
        let p = make_provider(ctx, name, *ver, coverage);
        ctx.set_array_element(arr, i, Value::Object(Some(p?)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn security_get_providers_filtered(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let filter = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let Some((type_str, algorithm)) = filter.split_once('.') else {
        return Ok(Some(Value::Object(None)));
    };
    if type_str.is_empty() || algorithm.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let matches: Vec<(String, f64, &'static str)> = snapshot()
        .into_iter()
        .filter(|(name, _, _)| get_service_entry(name, type_str, algorithm).is_some())
        .collect();
    if matches.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, matches.len());
    for (i, (name, ver, coverage)) in matches.iter().enumerate() {
        let provider = make_provider(ctx, name, *ver, coverage);
        ctx.set_array_element(arr, i, Value::Object(Some(provider?)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn security_get_provider(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name_str = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    Ok(Some(match provider_object_named(ctx, &name_str)? {
        Some(p) => Value::Object(Some(p)),
        None => Value::Object(None),
    }))
}

/// The installed `Provider` object for `name`, or `None` when no provider by
/// that name is installed — the body of `Security.getProvider(String)`,
/// callable from a native that needs to ATTACH a provider rather than answer
/// one.
///
/// Extracted so `javax.net.ssl.SSLContext.getProvider()` cannot grow a second
/// copy of the "prefer the object the application actually registered" rule
/// below; that rule is what makes `assertSame(added, ...)` and
/// `instanceof BouncyCastleProvider` work, and a second implementation of it
/// would be a second chance to get it wrong.
pub(crate) fn provider_object_named(
    ctx: &mut dyn NativeContext,
    name_str: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    match find(name_str) {
        Some((ver, coverage)) => {
            // A provider the application registered itself must come back as
            // THE SAME OBJECT it passed to `Security.addProvider`. The JDK's
            // provider list stores the instance, so code that installs a
            // provider and reads it back compares by identity or by class:
            // netty's `BouncyCastleUtilTest` asserts `assertSame(added,
            // Security.getProvider("BC"))`, and `BouncyCastleUtil` decides
            // whether BouncyCastle is present by testing the answer with
            // `instanceof BouncyCastleProvider`. Handing back a fresh
            // `make_provider` synthetic failed both: the caller saw a bare
            // `java.security.Provider` where it had registered a
            // `BouncyCastleProvider`, and any provider-private state (BC's own
            // service/creator maps) was unreachable through it. The registered
            // object is already pinned by `remember_real_provider`, so
            // preferring it costs no extra rooting.
            if let Some(real) = resolve_real_provider(ctx, name_str) {
                return Ok(Some(real));
            }
            Ok(Some(make_provider(ctx, name_str, ver, coverage)?))
        }
        // JDK contract: return null for unknown name.
        None => Ok(None),
    }
}

/// Read a Provider receiver's `(name, version)` honouring the real-JDK
/// field layout (`name:String`, `version:double`) and falling back to
/// our synthetic slots 0/1 when the receiver is a `make_provider`-style
/// synthetic.  Returns `None` if the name slot is null/empty (matches
/// the previous "no-op on bogus arg" behaviour).
fn read_provider_name_version(ctx: &dyn NativeContext, prov: ObjectRef) -> Option<(String, f64)> {
    let name = {
        let by_name = ctx.get_field_by_name(prov, "name");
        let n = match by_name {
            Value::Object(Some(s)) => Some(s),
            _ => match ctx.get_field(prov, 0) {
                Value::Object(Some(s)) => Some(s),
                _ => None,
            },
        };
        match n {
            Some(s) => ctx.read_string(s).unwrap_or_default(),
            None => return None,
        }
    };
    if name.is_empty() {
        return None;
    }
    let ver = match ctx.get_field_by_name(prov, "version") {
        Value::Double(d) => d,
        Value::Int(i) => i as f64,
        _ => match ctx.get_field(prov, 1) {
            Value::Double(d) => d,
            Value::Int(i) => i as f64,
            _ => 1.0,
        },
    };
    Some((name, ver))
}

fn security_add_provider(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let prov = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let (name, ver) = match read_provider_name_version(ctx, prov) {
        Some(pair) => pair,
        None => return Ok(Some(Value::Int(-1))),
    };
    remember_real_provider(ctx, &name, prov);
    Ok(Some(Value::Int(add(name, ver))))
}

fn security_insert_provider_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let prov = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let pos = args
        .get(1)
        .and_then(|v| {
            if let Value::Int(i) = v {
                Some(*i)
            } else {
                None
            }
        })
        .unwrap_or(1);
    let (name, ver) = match read_provider_name_version(ctx, prov) {
        Some(pair) => pair,
        None => return Ok(Some(Value::Int(-1))),
    };
    remember_real_provider(ctx, &name, prov);
    Ok(Some(Value::Int(insert_at(name, ver, pos))))
}

fn security_remove_provider(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    remove(&name);
    if let Some(handle) = real_provider_table().lock().remove(&name) {
        ctx.remove_global_root(handle);
    }
    Ok(None)
}

/// `Security.getProperty(String)` — minimal seed of well-known keys
/// consulted by `MessageDigest`/`KeyStore`/`SSL*Factory` bootstrap. Anything
/// else returns `null` to match HotSpot semantics: BouncyCastle and other
/// libraries do `if (val != null) val.substring(...)` checks, and an empty
/// string would cause `StringIndexOutOfBoundsException` (e.g. PKCS12$Mappings
/// reads `org.bouncycastle.pkcs12.default` and unconditionally substrings).
fn security_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let key = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    // A `Security.setProperty` override wins over the seeded default, and
    // makes a key that has no default answerable at all (real
    // `Security.getProperty` is backed by a mutable Properties, not a fixed
    // table). See `security_property_overrides`.
    if let Some(v) = security_property_overrides().lock().get(&key).cloned() {
        let s = ctx.create_string(&v);
        return Ok(Some(Value::Object(Some(s))));
    }
    let val = match key.as_str() {
        "securerandom.source" => "file:/dev/urandom",
        // LOWERCASE, matching the JDK's own `conf/security/java.security`,
        // which ships `keystore.type=pkcs12`. MEASURED in both modes
        // (`probes/KeyStoreFamilySweep.java`): `KeyStore.getDefaultType()` is
        // specified as `Security.getProperty("keystore.type")` verbatim, so
        // this table's spelling IS the method's answer -- HotSpot "pkcs12",
        // this VM "PKCS12". Harmless to `KeyStore.getInstance`, which is
        // case-insensitive; not harmless to the caller that compares the
        // default type against a literal, which is the ordinary way to ask
        // "am I on the default store type".
        "keystore.type" => "pkcs12",
        "ssl.KeyManagerFactory.algorithm" => "SunX509",
        "ssl.TrustManagerFactory.algorithm" => "PKIX",
        // Not one of this VM's four deliberate answers: ask the JDK's own
        // `conf/security/java.security`, which is right here, rather than
        // reporting "no such property" for the whole file.
        //
        // This arm was written and left UNWIRED for one release cycle, because
        // turning it on is not free: the stock file sets
        // `keystore.type.compat=true`, the gate BouncyCastle's
        // `AdaptingKeyStoreSpi` uses to probe a stream for JKS, and that path
        // then builds a PKCS#12 MAC through
        // `Mac.getInstance(name, providerObject)` — an overload that refused
        // every BouncyCastle name. Enabling the file first took `cert.test`
        // from PASS to FAIL. That overload is fixed now (see
        // `phases_late::ssl_security`, the `(String, Provider)` registration),
        // so the gate opens onto a path that works.
        _ => {
            return Ok(match java_security_file_property(ctx, &key) {
                Some(v) => {
                    let s = ctx.create_string(&v);
                    Some(Value::Object(Some(s)))
                }
                None => Some(Value::Object(None)),
            })
        }
    };
    let s = ctx.create_string(val);
    Ok(Some(Value::Object(Some(s))))
}

/// `Security.getProperty` for a key this VM does not answer itself, read from
/// the configured JDK's `conf/security/java.security`.
///
/// The four hardcoded answers above are deliberate (an unblocking
/// `securerandom.source`, in particular) and still win. Everything else used to
/// be `null`, which is not "no such property" — it is a whole configuration
/// file this VM declined to read, and library code reads it through
/// `Security.getProperty` all the time.
///
/// Measured: BouncyCastle's `AdaptingKeyStoreSpi` gates its JKS-compatibility
/// path on `Properties.isOverrideSet("keystore.type.compat")`, which resolves
/// through `Security.getProperty` first, and the stock file says
/// `keystore.type.compat=true`. Answering null took the PKCS12 path for a JKS
/// stream and threw `IOException: stream does not represent a PKCS12 key
/// store` — `PKCS12StoreTest.testJKS`, which HotSpot passes.
///
/// Parsed once. `java.security` is `key=value` with `#` comments and no
/// sections; a continuation-free read is enough for the lookups callers make,
/// and a file that cannot be read leaves every key unanswered exactly as before.
fn java_security_file_property(ctx: &mut dyn NativeContext, key: &str) -> Option<String> {
    // LOCK LEVEL (lock-discipline ratchet): `Scratch`. That level is a claim
    // that no call back into the VM happens under this guard, and the parse
    // below calls `ctx.get_system_property`. So the parse runs OUTSIDE the
    // lock and only the publish is taken under it.
    //
    // Two threads that miss together both parse; `get_or_insert` keeps the
    // first and drops the second. Behaviour-preserving — the file is read-only
    // and both parses produce the same map — and strictly cheaper than the
    // alternative of holding a lock across a filesystem read.
    static FILE_PROPS: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<
            Option<std::collections::HashMap<String, String>>,
        >,
    > = std::sync::OnceLock::new();
    let cell = FILE_PROPS.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            None,
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    });
    if let Some(answer) = {
        let guard = cell.lock();
        guard.as_ref().map(|m| m.get(key).cloned())
    } {
        return answer;
    }
    let mut parsed = std::collections::HashMap::new();
    if let Some(home) = ctx.get_system_property("java.home") {
        let path = std::path::Path::new(&home)
            .join("conf")
            .join("security")
            .join("java.security");
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    parsed.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
    }
    let mut guard = cell.lock();
    guard.get_or_insert(parsed).get(key).cloned()
}

/// Process-wide overrides written by `Security.setProperty`.
///
/// Stub-removal wave 2 (2026-07-27): `security_set_property` used to be a
/// no-op and `security_get_property` answered from a fixed four-entry table,
/// so `Security.setProperty("keystore.type", "JKS")` was silently discarded
/// and the very next `getProperty` still reported `PKCS12`. That is live in
/// the DEFAULT (real-JDK) build — `register_jca_natives` is reached from
/// `register_essential_natives_with_shims` — and it is what makes real
/// `KeyStore.getDefaultType()` bytecode unconfigurable, since that method is
/// specified as `Security.getProperty("keystore.type")`.
///
/// `phases_early::security_properties()` is the same idea but is registered
/// only from `register_synthetic_overrides`; the two are never both
/// authoritative (phases_early registers later, so it wins under
/// `--synthetic-jdk`; only this one exists in the default build).
fn security_property_overrides(
) -> &'static parking_lot::Mutex<std::collections::HashMap<String, String>> {
    static OVERRIDES: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<String, String>>,
    > = std::sync::OnceLock::new();
    OVERRIDES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn security_set_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Real `Security.setProperty` NPEs on a null key or value.
    let key = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("key can't be null".to_string()),
            }
            .into())
        }
    };
    let val = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("datum can't be null".to_string()),
            }
            .into())
        }
    };
    security_property_overrides().lock().insert(key, val);
    Ok(None)
}

// ---------------------------------------------------------------------------
// WP6.5 — `java.security.Provider$Service.<init>` native shim.
//
// ## Problem
//
// `bench/wildfly/bcprobe.stderr.log` shows BouncyCastleProvider's setup
// chain NPEing inside `Provider$Service.<init>` at pc=29:
//
//     class=java/security/Provider$Service method=<init> pc=29
//     NullPointerException("Cannot invoke get on null")
//
// pc=29 maps to `this.engineDescription = knownEngines.get(type);` in
// the OpenJDK 21+ bytecode for the constructor.  `knownEngines` is a
// static `Map<String,EngineDescription>` populated by `Provider.<clinit>`
// — but the populating sequence depends on the inner classes
// `Provider$ServiceKey` and `EngineDescription` resolving cleanly under
// real-JDK loading, which the cratonvm class manager doesn't fully wire
// today.  Result: `knownEngines` stays null, every BC `addAlgorithm`
// call constructs a `Provider$Service` and the constructor NPEs on the
// `.get(...)` call before BC finishes registering its ~500 algorithm
// mappings.
//
// ## Fix
//
// Replace the constructor with a native that copies the six argument
// references straight into the receiver's instance fields.  Bypassing
// the bytecode means `knownEngines` is never read; the resulting
// `Service` instance has `engineDescription == null` (the field is
// final but the JVM allocates it as null and never writes via
// PUTFIELD), which is fine for downstream code as long as nobody does
// `service.engineDescription.foo()` (verified by audit — the
// `engineDescription` field is consumed only inside `newInstance` /
// `supportsParameter` paths that we either intercept upstream
// (`Cipher.getInstance` / `Signature.getInstance`) or that fail
// gracefully when the description is null).
//
// ## Field layout strategy
//
// The OpenJDK 21+ source declaration order for `Provider$Service` is:
//   final Provider provider;        // declared first
//   final String   type;            // engine type
//   final String   algorithm;
//   final String   className;
//   final List<String>          aliases;
//   final Map<String,String>    attributes;
//   final EngineDescription     engineDescription;  // we LEAVE NULL
//
// Two write paths cover both modes:
//   1. `set_field_by_name` — resolves the slot from the class hierarchy
//      so real-JDK Provider$Service writes go to the right offsets.
//   2. Slot fallback — `phases_early::register_phase53_security`
//      registers synthetic getters reading `(type=0, algorithm=1,
//      provider=2)`.  We mirror writes to those slots so synthetic-mode
//      callers keep working.
//
// Constructor signature (instance method, args[0]=this):
//   `(Provider, String type, String algorithm, String className,
//     List<String> aliases, Map<String,String> attributes)V`
//
// args[0]=this  args[1]=provider  args[2]=type      args[3]=algorithm
// args[4]=className args[5]=aliases  args[6]=attributes
// ---------------------------------------------------------------------------

fn empty_collection_value(
    ctx: &mut dyn NativeContext,
    method: &str,
    descriptor: &str,
    fallback_class: &str,
) -> Result<Value, MethodCallFailed> {
    if let Ok(Some(Value::Object(Some(obj)))) =
        ctx.invoke("java/util/Collections", method, descriptor, &[])
    {
        return Ok(Value::Object(Some(obj)));
    }

    let cid = ctx.ensure_class_initialized(fallback_class).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("Provider$Service: {fallback_class} not loaded"),
        })
    })?;
    let obj = ctx.alloc_object(cid, ctx.class_num_total_fields(cid).max(4));
    let obj_pin = ctx.pin_native_root(obj);
    ctx.invoke(fallback_class, "<init>", "()V", &[Value::Object(Some(obj))])?;
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Ok(Value::Object(Some(obj)))
}

/// Did the `set_field_by_name("provider", …)` write actually land — i.e. does
/// this `Provider$Service` have the REAL JDK layout rather than CratonVM's
/// synthetic one?
///
/// The two layouts disagree by a one-slot rotation. Synthetic:
/// `(type=0, algorithm=1, provider=2, className=3)`, matching the accessors
/// `phases_early::register_phase53_security` registers. Real JDK declaration
/// order: `(provider=0, type=1, algorithm=2, className=3)`. So mirroring the
/// synthetic slots onto a real `Service` writes the `type` String over
/// `provider`, `provider` over `algorithm`, and undoes every named write.
///
/// It stayed invisible for as long as every JCA engine CratonVM served was
/// intercepted upstream of the JDK's own `GetInstance`. `CertStore.getInstance`
/// is not intercepted — it runs `new CertStore(spi, instance.provider, …)` on
/// real bytecode — so `CertStore.getProvider()` handed back the type String and
/// `.getName()` on it threw
/// `NoSuchMethodError: java.lang.String.getName()Ljava/lang/String;`.
///
/// Read-back beats a layout query here because it answers the question the
/// caller actually has ("did my write take?") in both modes, with no new
/// `NativeContext` surface.
fn service_has_named_layout(
    ctx: &mut dyn NativeContext,
    service: ObjectRef,
    provider: ObjectRef,
) -> bool {
    matches!(
        ctx.get_field_by_name(service, "provider"),
        Value::Object(Some(got)) if got == provider
    )
}

fn provider_service_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    let provider = args.get(1).copied().unwrap_or(Value::Object(None));
    let svc_type = args.get(2).copied().unwrap_or(Value::Object(None));
    let algorithm = args.get(3).copied().unwrap_or(Value::Object(None));
    let class_name = args.get(4).copied().unwrap_or(Value::Object(None));
    let class_name_text = match class_name {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let mut aliases = args.get(5).copied().unwrap_or(Value::Object(None));
    let mut attributes = args.get(6).copied().unwrap_or(Value::Object(None));

    // Real-JDK path: write by field name so the actual class layout is
    // honoured.  Each call is a no-op when the receiver has no field with
    // that name (synthetic mode).
    ctx.set_field_by_name(this, "provider", provider);
    ctx.set_field_by_name(this, "type", svc_type);
    ctx.set_field_by_name(this, "algorithm", algorithm);
    ctx.set_field_by_name(this, "className", class_name);
    // `engineDescription` left null — see module doc; this is the whole
    // point of the shim.

    // Synthetic-mode mirror: `phases_early::register_phase53_security`
    // exposes `getType` / `getAlgorithm` / `getProvider` as slot-index
    // reads at 0/1/2.  Mirror those writes so synthetic Provider$Service
    // allocations (i.e. when the real-JDK class isn't loaded) keep
    // returning the right values.
    //
    // ONLY in synthetic mode. The two layouts disagree — the synthetic one is
    // `(type=0, algorithm=1, provider=2, className=3)`, the real JDK's
    // declaration order is `(provider=0, type=1, algorithm=2, className=3)` —
    // so on a real `Provider$Service` this mirror wrote `type` over `provider`,
    // `algorithm` over `type` and `provider` over `algorithm`, undoing all
    // three `set_field_by_name` writes above with a one-slot rotation.
    //
    // That was invisible while every JCA engine CratonVM served was intercepted
    // before the JDK's own `GetInstance` ran. `CertStore.getInstance` is not:
    // it runs real bytecode that does `new CertStore(spi, instance.provider, …)`
    // and `CertStore.getProvider()` then returned the *type* String, so
    // `getProvider().getName()` died as
    // `NoSuchMethodError: java.lang.String.getName()` — the absurd-receiver
    // shape [[a-native-must-not-write-a-field-of-a-receiver-it-did-not-build]]
    // describes.
    //
    // Detected by reading back one of the named writes rather than by asking
    // for the class layout — see `service_has_named_layout`.
    let named_layout = match provider {
        Value::Object(Some(p)) => service_has_named_layout(ctx, this, p),
        // A null provider cannot be read back distinguishably. Fall back to
        // asking whether the receiver kept the `className` we just wrote.
        _ => {
            matches!(
                ctx.get_field_by_name(this, "className"),
                Value::Object(Some(_))
            ) && matches!(class_name, Value::Object(Some(_)))
        }
    };
    if !named_layout {
        let nfields = ctx.object_num_fields(this);
        if nfields > 0 {
            ctx.set_field(this, 0, svc_type);
        }
        if nfields > 1 {
            ctx.set_field(this, 1, algorithm);
        }
        if nfields > 2 {
            ctx.set_field(this, 2, provider);
        }
        if nfields > 3 {
            ctx.set_field(this, 3, class_name);
        }
    }

    // OpenJDK Provider.Service turns null aliases/attributes into immutable
    // empty collections. Real provider constructors pass null for the common
    // no-alias/no-attribute case, and Provider.putService immediately iterates
    // `getAliases()` and `attributes.entrySet()`. Leaving either field null
    // makes JDK providers fail during construction.
    let this_pin = ctx.pin_native_root(this);
    if matches!(aliases, Value::Object(None)) {
        aliases = empty_collection_value(
            ctx,
            "emptyList",
            "()Ljava/util/List;",
            "java/util/ArrayList",
        )?;
    }
    let this_now = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this_now, "aliases", aliases);
    if matches!(attributes, Value::Object(None)) {
        attributes =
            empty_collection_value(ctx, "emptyMap", "()Ljava/util/Map;", "java/util/HashMap")?;
    }
    let this_now = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this_now, "attributes", attributes);
    if !class_name_text.is_empty() {
        let ih = ctx.identity_hash_code(this_now) as i64;
        service_classname_table().lock().insert(ih, class_name_text);
    }
    ctx.unpin_native_roots(this_pin);

    Ok(None)
}

/// `<clinit>` no-op — used to mark a class as initialized without
/// running its bytecode.  Identical body to `cipher::clinit_noop`,
/// restated here so this module is self-contained.
#[inline]
fn clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// WP6.5 finish — `Provider.put` / `parseLegacyPut` / `getService`.
//
// ## What this section adds
//
// A process-wide service registry keyed on `(provider_name, type,
// algorithm_normalized)` plus an alias map keyed on
// `(provider_name, type, alias_normalized) → canonical algorithm`.
// The registry is populated by `Provider.put(Object, Object)` —
// BouncyCastleProvider's `addAlgorithm(...)` chain reaches it via
// the JDK's `parseLegacyPut(String, String)` helper which cracks
// "Cipher.AES/GCM/NoPadding" into `(type="Cipher", algo="AES/GCM/NoPadding")`.
//
// ## What it solves
//
// Without a per-provider service map, `Cipher.getInstance(algo, "BC")`
// has no way to confirm BC actually registered the algorithm — every
// such call would either fall through to permissive synthetic dispatch
// (which silently misroutes BC-only algos to the SUN-style backend) or
// throw NoSuchAlgorithmException because real-JDK Provider.getService
// queries `services` which our shim leaves null.
//
// ## Service entry shape
//
// `ServiceEntry` mirrors the six fields a JDK `Provider.Service`
// constructor takes — `provider`, `type`, `algorithm`, `className`,
// `aliases`, `attributes` — but stores only what the consumer side
// (`Cipher.getInstance(algo, providerName)`, `Provider.getService(...)`)
// reads back.  We don't materialise `EngineDescription` (the
// `<init>` shim leaves it null on purpose, see module-level docs).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct ServiceEntry {
    /// Engine type, e.g. "Cipher", "MessageDigest", "Signature".
    /// Stored exactly as the put() call passed it (case preserved
    /// for diagnostics, but lookups go through `normalize_engine`).
    type_str: String,
    /// Canonical algorithm name, e.g. "AES/GCM/NoPadding".  Lookups
    /// route through `normalize_algo`.
    algorithm: String,
    /// Implementation class name, e.g.
    /// "org.bouncycastle.jcajce.provider.symmetric.AES$GCM".
    /// May be empty if the put() call only registered an alias.
    class_name: String,
    /// Original lookup key used during `put` (e.g. the literal
    /// "Cipher.AES/GCM/NoPadding"), retained so `getService` /
    /// debugging can render it verbatim.
    key: String,
    /// The service's attributes, as `("SupportedCurves", "...")` pairs — the
    /// `Type.Algorithm AttrName` legacy rows. Kept in insertion order rather
    /// than a map because there are a handful per service and the order is
    /// what a provider's own listing shows.
    attributes: Vec<(String, String)>,
}

/// Process-wide service map, keyed `(provider_name → (type, algo) → entry)`.
/// Provider-name match is case-sensitive (matches JDK semantics — names
/// are stable identifiers).  Type+algo lookups normalise to ASCII upper-
/// case so `cipher.getInstance("aes/gcm/nopadding", "BC")` resolves the
/// same entry as `getInstance("AES/GCM/NoPadding", "BC")`.
type ServiceMap = FxHashMap<(String, String), ServiceEntry>;

fn services() -> &'static parking_lot::Mutex<FxHashMap<String, ServiceMap>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<parking_lot::Mutex<FxHashMap<String, ServiceMap>>> = OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

/// Process-wide alias map, keyed `(provider_name, type, alias_norm)
/// → canonical_algo_norm`.  BouncyCastle's legacy `addAlgorithm` flow
/// produces both kinds of put-keys; we resolve aliases on the way in,
/// not on the way out, so `getService` is a single hash lookup.
/// One `Alg.Alias.<Type>.<Alias>` row.
///
/// The lookup KEY normalises type and alias to upper case, because lookups are
/// case-insensitive. The row keeps both raw spellings anyway, for the same
/// reason [`ServiceEntry`] keeps `type_str` and `algorithm`: a provider's
/// property view has to render `Alg.Alias.MessageDigest.SHA1`, and
/// `ALG.ALIAS.MESSAGEDIGEST.SHA1` is not a key any caller would recognise.
/// Recovering the spelling from the normalised key is not possible — uppercase
/// is lossy, and `TripleDES` is the counter-example that proves it.
#[derive(Clone, Debug, Default)]
struct AliasEntry {
    /// The canonical algorithm, in the provider's own spelling.
    canonical: String,
    /// Engine type as the `put` spelled it, e.g. `"MessageDigest"`.
    type_str: String,
    /// Alias as the `put` spelled it, e.g. `"SHA1"`.
    alias: String,
}

fn aliases() -> &'static parking_lot::Mutex<FxHashMap<(String, String, String), AliasEntry>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<parking_lot::Mutex<FxHashMap<(String, String, String), AliasEntry>>> =
        OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

/// Process-wide RAW provider properties, keyed `(provider_name, exact_put_key)
/// → value`. Mirrors what a real `Provider`'s inherited `Properties` table
/// holds: every `put`/`parseLegacyPut(key, value)` is recorded verbatim here so
/// `Provider.getProperty(key)` returns it. Needed because our `put` natives
/// capture into the structured service/alias maps but never populate the
/// synthetic Provider's real Hashtable, and real JDK code (e.g. BouncyCastle's
/// `X509SignatureUtil.lookupAlg` → `Security.getProvider("BC").getProperty(
/// "Alg.Alias.Signature.OID.<oid>")`) reads aliases back via `getProperty`.
/// Keyed by provider *name* (not object) so it survives the fresh synthetic
/// `Provider` instance handed out by each `Security.getProvider` call.
fn provider_properties() -> &'static parking_lot::Mutex<FxHashMap<(String, String), String>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<parking_lot::Mutex<FxHashMap<(String, String), String>>> = OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

/// Per-INSTANCE raw keys a `Provider` object has itself `put`/`putService`d,
/// keyed `(identity_hash_of_receiver, exact_put_key) → ()`. `identity_hash_code`
/// is GC-move-stable (header word seeded on first read — see
/// `native-collections/src/identity_hash.rs` and `System.identityHashCode`'s
/// native), unlike a raw `ObjectRef`/pointer, so this table stays correct
/// across a moving-GC relocation of the receiver.
///
/// Why this exists alongside the name-keyed `provider_properties()` table:
/// that table is intentionally shared across every `Provider` object with the
/// same name, because `Security.getProvider(name)` hands back a *fresh*
/// synthetic object on every call (`make_provider` above) and property reads
/// (`getProperty`/aliases) need to see what any same-named instance wrote.
/// But real providers that are constructed directly by Java code (never
/// routed through `Security.addProvider`) — e.g. BouncyCastle's own
/// `BCJcaJceHelper` internally does `new BouncyCastleProvider()` on top of
/// whatever instance the caller already cached elsewhere — are genuinely
/// separate objects with independent `legacyMap`/`serviceMap` state on real
/// HotSpot. `BouncyCastleProvider.addAlgorithm` calls `containsKey(key)`
/// *before* registering each algorithm and throws `IllegalStateException:
/// duplicate provider key` if it's already present — real JDK never trips
/// this for a second `new BouncyCastleProvider()` because its per-instance
/// map starts empty, but our shared-by-name table did, spuriously (see
/// `provider_put_service_native` doc comment for the concrete repro this
/// fixed). Scoping `containsKey`'s view to the receiver's own identity hash
/// restores per-instance isolation while leaving the name-keyed table (and
/// every other consumer of it) untouched.
fn provider_instance_keys() -> &'static parking_lot::Mutex<std::collections::HashSet<(i64, String)>>
{
    use std::sync::OnceLock;
    static SET: OnceLock<parking_lot::Mutex<std::collections::HashSet<(i64, String)>>> =
        OnceLock::new();
    SET.get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()))
}

/// Engine type normalisation: ASCII uppercase, no leading/trailing dots.
/// JDK's `Provider$ServiceKey` uses case-insensitive comparison for both
/// type and algorithm.
fn normalize_engine(s: &str) -> String {
    s.trim().to_ascii_uppercase()
}

/// Algorithm-name normalisation: ASCII uppercase.  The JDK trims spaces
/// inside transformation strings on lookup but BC stores them with no
/// internal whitespace, so simple uppercase suffices.
fn normalize_algo(s: &str) -> String {
    s.trim().to_ascii_uppercase()
}

/// Provider-side getter: fetch `(class_name, algorithm)` for an
/// `(provider, type, algo-or-alias)` triple.  Caller-side aliases are
/// resolved here before the service lookup runs, so e.g.
/// `getService("Cipher", "AES")` will resolve to the entry whose
/// canonical algo is "AES" even if BC registered an alias chain.
fn get_service_entry(provider: &str, type_str: &str, algo: &str) -> Option<ServiceEntry> {
    let type_n = normalize_engine(type_str);
    let algo_n = normalize_algo(algo);
    // Resolve alias first. The alias table stores the canonical name in the
    // provider's OWN spelling (`Alg.Alias.KeyFactory.<oid> = EC` records "EC",
    // `Alg.Alias.KeyFactory.DH = DiffieHellman` records "DiffieHellman"), so the
    // service-map key has to be re-normalised here rather than at insertion —
    // the raw spelling is what `canonical_service_algorithm` hands to the native
    // engines, which is the only form a caller-facing `getAlgorithm()` may echo.
    let canonical = aliases()
        .lock()
        .get(&(provider.to_string(), type_n.clone(), algo_n.clone()))
        .map(|e| normalize_algo(&e.canonical))
        .unwrap_or(algo_n);
    services()
        .lock()
        .get(provider)
        .and_then(|m| m.get(&(type_n, canonical)).cloned())
}

/// The canonical service name a provider's `Alg.Alias.<Type>.<name>` row maps
/// `algo` onto, in that provider's own spelling — `None` when `algo` is already
/// a primary name, or when no alias row covers it.
///
/// `provider` `None` searches the chain in order, mirroring the anonymous
/// `getInstance` walk.
///
/// This exists because the engines that intercept `getInstance` natively —
/// `KeyFactory`, `KeyPairGenerator`, `MessageDigest`, `Mac`, `SecretKeyFactory`,
/// `AlgorithmParameters` — gate on a hand-written name table and never consult a
/// registry, so an alias spelling that `Provider.getService` resolves perfectly
/// well was refused one layer up. Every X.509/PKCS/CMS caller names algorithms
/// by OID, and an OID is exactly an `Alg.Alias` row: `KeyFactory.getInstance(
/// "1.2.840.10045.2.1", "BC")` raised `NoSuchAlgorithmException` while
/// `bc.get("Alg.Alias.KeyFactory.1.2.840.10045.2.1")` answered `"EC"` on the
/// same VM, in the same process, from the table this reads.
pub(crate) fn canonical_service_algorithm(
    provider: Option<&str>,
    type_str: &str,
    algo: &str,
) -> Option<String> {
    let type_n = normalize_engine(type_str);
    let algo_n = normalize_algo(algo);
    let table = aliases().lock();
    let lookup = |name: &str| -> Option<String> {
        table
            .get(&(name.to_string(), type_n.clone(), algo_n.clone()))
            .map(|e| e.canonical.clone())
    };
    match provider {
        Some(p) => lookup(p),
        None => {
            drop(table);
            let names = snapshot();
            let table = aliases().lock();
            names.into_iter().find_map(|(name, _, _)| {
                table
                    .get(&(name, type_n.clone(), algo_n.clone()))
                    .map(|e| e.canonical.clone())
            })
        }
    }
}

/// [`canonical_if_unrecognised`] for the ANONYMOUS overloads, where the rewrite
/// may only come from a provider this VM implements natively.
///
/// An alias row is owned by the provider that registered it, and it means that
/// provider's implementation. `Alg.Alias.Mac.2.16.840.1.101.3.4.2.1` is
/// BouncyCastle's, and it names BouncyCastle's `SHA256$HashMac` — a PKCS#12
/// capable MAC that derives its key from a `PKCS12Key` plus a
/// `PBEParameterSpec`. Rewriting the caller's OID to `HmacSHA256` on the
/// strength of that row and then serving it from this crate's plain HMAC
/// borrows one provider's NAME to reach another's IMPLEMENTATION, and the two
/// are not the same function — measured, they disagree on every byte.
///
/// The cost was silent and asymmetric. BouncyCastle's
/// `JcePKCS12MacCalculatorBuilder` builds its MAC through the ANONYMOUS
/// `Mac.getInstance(oid)` while `JcePKCS12MacCalculatorBuilderProvider` names
/// BC, so a PKCS#12 file got a MAC from this engine and was then verified
/// against BouncyCastle's: `PfxPduTest.testCreateAES256andSHA256`, where the
/// stored and recomputed MacData differed for SHA-256 and agreed for SHA-1
/// (SunJCE owns neither OID; the SHA-1 one simply had no rewrite this engine
/// accepted).
///
/// A provider outside `NATIVELY_SERVICED_PROVIDERS` therefore does not get to
/// rename anything here; the caller's own spelling goes to the chain instead,
/// which hands the call to the provider that owns it.
pub(crate) fn canonical_if_unrecognised_native_only(
    type_str: &str,
    algo: &str,
    recognised: &dyn Fn(&str) -> bool,
) -> Option<String> {
    if recognised(algo) {
        return None;
    }
    let type_n = normalize_engine(type_str);
    let algo_n = normalize_algo(algo);
    let names = snapshot();
    let table = aliases().lock();
    let canonical = names.into_iter().find_map(|(name, _, _)| {
        if !NATIVELY_SERVICED_PROVIDERS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(&name))
        {
            return None;
        }
        table
            .get(&(name, type_n.clone(), algo_n.clone()))
            .map(|e| e.canonical.clone())
    })?;
    drop(table);
    recognised(&canonical).then_some(canonical)
}

/// `algo` rewritten to the canonical name a native engine recognises, or `None`
/// when no rewrite helps.
///
/// `recognised` is the engine's own gate — the very predicate whose refusal sent
/// us here. Both halves are tested: a caller spelling the engine already knows
/// is returned unchanged (`None`), and a canonical form the engine STILL does
/// not know is not substituted either, so the caller's original name is what
/// appears in the refusal message instead of a resolved one it never typed.
pub(crate) fn canonical_if_unrecognised(
    provider: Option<&str>,
    type_str: &str,
    algo: &str,
    recognised: &dyn Fn(&str) -> bool,
) -> Option<String> {
    if recognised(algo) {
        return None;
    }
    let canonical = canonical_service_algorithm(provider, type_str, algo)?;
    recognised(&canonical).then_some(canonical)
}

/// The provider name a `getInstance(algorithm, X)` overload's argument `idx`
/// names — the string itself for the `(String, String)` form, the provider's own
/// `getName()` for the `(String, Provider)` form, and `None` for the anonymous
/// single-argument form.
pub(crate) fn provider_arg_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> Option<String> {
    let Some(Value::Object(Some(obj))) = args.get(idx) else {
        return None;
    };
    let is_string = ctx
        .class_name_of_id(ctx.class_id_of_object(*obj))
        .is_some_and(|n| n == "java/lang/String");
    let name = if is_string {
        ctx.read_string(*obj).unwrap_or_default()
    } else {
        provider_name_of(ctx, *obj)
    };
    if name.is_empty() || name == "<unknown>" {
        None
    } else {
        Some(name)
    }
}

/// Record the provider a caller explicitly named onto the engine object's own
/// real `provider` field, so `getProvider()` and the real JDK `toString()`
/// bytecode both answer it.
///
/// The native engines build their answer from a name table keyed on the
/// ALGORITHM (`kf_provider_name` and friends), which can only ever name the JDK
/// provider that would have served it — so `KeyFactory.getInstance("EC", "BC")`
/// reported `SunEC`, `Cipher.getInstance(t, "BC")` reported `SunJCE`, and
/// `Cipher.toString()`/`MessageDigest.toString()`, which read this field
/// directly, reported `(no provider)` because nothing ever wrote it. HotSpot
/// answers `BC` for all four. Only ever called for an explicitly-named provider:
/// the anonymous overload genuinely IS served by the JDK provider the name table
/// names, and must keep answering it.
pub(crate) fn record_requested_provider(
    ctx: &mut dyn NativeContext,
    engine_obj: ObjectRef,
    provider: &str,
) {
    let Some((ver, coverage)) = find(provider).or_else(|| Some((25.0, USER_PROVIDER_COVERAGE)))
    else {
        return;
    };
    let pin = ctx.pin_native_root(engine_obj);
    let prov_obj = make_provider(ctx, provider, ver, coverage);
    let engine_obj = ctx.read_native_pin(pin, engine_obj);
    ctx.unpin_native_roots(pin);
    if let Ok(prov) = prov_obj {
        ctx.set_field_by_name(engine_obj, "provider", Value::Object(Some(prov)));
    }
}

/// The `Provider` recorded by [`record_requested_provider`], if any.
pub(crate) fn recorded_requested_provider(
    ctx: &mut dyn NativeContext,
    engine_obj: ObjectRef,
) -> Option<ObjectRef> {
    match ctx.get_field_by_name(engine_obj, "provider") {
        Value::Object(Some(p)) => Some(p),
        _ => None,
    }
}

/// Construct a THIRD-PARTY provider's own engine object for `(type_str, algo)`
/// when its registered class is itself a `required_super` — the shape
/// `KeyPairGenerator.getInstance` and `MessageDigest.getInstance` return
/// directly on HotSpot, because those two engines' SPI base classes extend the
/// engine class itself (`KeyPairGeneratorSpi` is `KeyPairGenerator`'s own
/// superclass surface; BouncyCastle's `BCMessageDigest extends MessageDigest`).
///
/// `Ok(None)` = not this provider's, or its class is not of that shape, and the
/// caller's own path stands. An `Err` means the provider owns the name and its
/// class would not construct — reported rather than hidden behind our engine.
///
/// The returned object gets its `algorithm` and `provider` fields written, as
/// `getInstance` does on HotSpot: those two accessors are `final` on the engine
/// class, so they are served from here (or from this crate's natives reading
/// these same fields) no matter what the provider's subclass overrides.
/// The JDK's own `<Engine>$Delegate` wrapper for an engine class, as
/// `(internal class name, `(Spi, String)` constructor descriptor)`.
///
/// Verified against JDK 25 with `javap -p`, not assumed: the ctor is
/// package-private and its sibling `(GetInstance$Instance, Iterator, String)`
/// overload is the one `getInstance` normally uses, so the two-argument form has
/// to be named exactly.
fn engine_delegate_shape(engine_class: &str) -> Option<(&'static str, &'static str)> {
    match engine_class {
        "java/security/KeyPairGenerator" => Some((
            "java/security/KeyPairGenerator$Delegate",
            "(Ljava/security/KeyPairGeneratorSpi;Ljava/lang/String;)V",
        )),
        _ => None,
    }
}

/// The same wrapper for an engine whose `Delegate` constructor takes the
/// `Provider` as well — `(Spi, String, Provider)`, in THAT order.
///
/// This is not a stylistic variant of [`engine_delegate_shape`]; it is a
/// different constructor arity, and until 2026-08-20 the `_ => None` arm above
/// was the whole story. Every engine whose `Delegate` needs the provider fell
/// off it and `build_third_party_engine` returned `Ok(None)`, so a provider
/// written to the DOCUMENTED JCA contract — a class `extends MessageDigestSpi`,
/// registered with `put("MessageDigest.X", …)` — was unreachable:
///
/// ```text
/// MessageDigest.getInstance("H13MD", "H13Prov")
///   HotSpot 25.0.3+9 -> java.security.MessageDigest$Delegate
///   CratonVM         -> NoSuchAlgorithmException: no such algorithm: H13MD
///                       for provider H13Prov
/// ```
///
/// (MEASURED, `docs/known-issues/jdk-only/H13-2-*.md` §2.) Only providers of
/// BouncyCastle's shape — where the registered class extends the ENGINE, e.g.
/// `BCMessageDigest extends MessageDigest` — ever reached the digest engine,
/// which is why the gap survived a bc-java-driven fix round: the corpus that
/// exercised this path had no standard-shaped provider in it.
///
/// Verified against JDK 25.0.3+9 with `javap -p -s`, not assumed:
/// `java.security.MessageDigest$Delegate` has exactly one constructor,
/// `private (MessageDigestSpi, String, Provider)`, alongside a static factory
/// `of(MessageDigestSpi, String, Provider)` that only picks between `Delegate`
/// and `CloneableDelegate`. Constructing `Delegate` directly is the
/// non-cloneable half of that choice and is correct for any SPI; an SPI that
/// also implements `Cloneable` gets a `MessageDigest` whose `clone()` throws
/// `CloneNotSupportedException` where HotSpot would clone — recorded as the
/// known residual of this fix rather than silently accepted.
fn engine_delegate_shape_with_provider(engine_class: &str) -> Option<(&'static str, &'static str)> {
    match engine_class {
        "java/security/MessageDigest" => Some((
            "java/security/MessageDigest$Delegate",
            "(Ljava/security/MessageDigestSpi;Ljava/lang/String;Ljava/security/Provider;)V",
        )),
        _ => None,
    }
}

pub(crate) fn build_third_party_engine(
    ctx: &mut dyn NativeContext,
    provider: &str,
    type_str: &str,
    algo: &str,
    required_super: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // Each of these four refusals used to be a bare `Ok(None)`, which the
    // caller reports as "this provider does not offer that algorithm" — the
    // `[_ => default]` shape this function's own doc names as the defect it
    // exists to remove. They are still refusals; they are no longer silent.
    if third_party_service_class(Some(provider), type_str, algo).is_none() {
        tracing::warn!(provider, type_str, algo, "jca chain: no third-party service class");
        return Ok(None);
    }
    let Some(impl_result) = build_jca_impl(ctx, provider, type_str, algo) else {
        tracing::warn!(provider, type_str, algo, "jca chain: build_jca_impl declined");
        return Ok(None);
    };
    let engine = match impl_result? {
        Some(Value::Object(Some(o))) => o,
        other => {
            tracing::warn!(provider, type_str, algo, ?other,
                "jca chain: the impl class did not construct an object");
            return Ok(None);
        }
    };
    // `class_id_by_name` only answers for a class the manager already HOLDS,
    // and the engine's `getInstance` is a registered native — calling it never
    // pulls `java.security.MessageDigest` (or `Signature`, or `KeyFactory`)
    // into the class manager. So this lookup could miss purely on LOAD ORDER,
    // and the refusal behind it reported the provider's algorithm as absent:
    // a fact about what had been loaded, presented as a fact about the
    // provider. MEASURED — `H13-2` §2's own falsifier fired here, three arms
    // past the delegate arm everyone (this file's doc comment included) assumed
    // was the culprit.
    let super_id = match ctx.class_id_by_name(required_super) {
        Some(id) => id,
        None => {
            let _ = ctx.load_class(required_super);
            match ctx.class_id_by_name(required_super) {
                Some(id) => id,
                None => {
                    tracing::warn!(provider, type_str, algo, required_super,
                        "jca chain: the engine superclass is not loadable even after                          an explicit load");
                    return Ok(None);
                }
            }
        }
    };
    // Did the provider hand back the ENGINE class itself, or a bare SPI we had
    // to wrap? The JDK's answer to that question decides who owns the
    // algorithm NAME, and it is not the caller — see the write at the end.
    let mut wrapped_a_bare_spi = false;
    let engine = if ctx.is_subclass(ctx.class_id_of_object(engine), super_id) {
        engine
    } else {
        wrapped_a_bare_spi = true;
        // The provider registered a BARE SPI. HotSpot wraps one in the engine's
        // package-private `Delegate`, and so do we — it is an ordinary JDK class
        // with a `(Spi, String)` constructor, and building it is what makes the
        // whole family reachable rather than only the providers whose SPI
        // happens to extend the engine class.
        //
        // BouncyCastle's composite-signature generators are exactly this shape:
        // `compositesignatures.KeyPairGeneratorSpi extends
        // java.security.KeyPairGeneratorSpi`, where its `ec.KeyPairGeneratorSpi$EC`
        // extends `java.security.KeyPairGenerator`. Declining here left
        // `MLDSA44-RSA2048-PKCS15-SHA256` (bc-java's `cert.cmp` suite)
        // unreachable while its sibling `EC` resolved.
        if let Some((delegate_class, delegate_desc)) = engine_delegate_shape(required_super) {
            let pin = ctx.pin_native_root(engine);
            let algo_str = ctx.create_string(algo);
            let spi = ctx.read_native_pin(pin, engine);
            ctx.unpin_native_roots(pin);
            match ctx.new_object_initialized(
                delegate_class,
                delegate_desc,
                &[Value::Object(Some(spi)), Value::Object(Some(algo_str))],
            ) {
                Ok(Some(Value::Object(Some(o)))) => o,
                // No Delegate on this JDK, or it would not construct. Leave the
                // caller on its own path rather than handing back a wrong type.
                _ => return Ok(None),
            }
        } else {
            // The `(Spi, String, Provider)` arity. Reached only by engines the
            // two-argument table above does not name, so nothing that works
            // today changes route: this arm can only turn a present-day
            // `Ok(None)` — and the caller's `NoSuchAlgorithmException` behind it
            // — into a working delegate.
            //
            // The `Provider` object is built the same way
            // `build_real_spi_wrapper` builds it, for the same reason: the
            // engine's `getProvider()` is `final`, so the wrapper's own
            // `provider` field is the only thing that can answer it. Note this
            // is a MADE provider object, not the instance the caller passed to
            // `Security.addProvider` — `getProvider() == myProvider` is `true`
            // on HotSpot and stays `false` here. That is a separate, older
            // defect (`H13-2` §4) and is deliberately not papered over.
            let Some((delegate_class, delegate_desc)) =
                engine_delegate_shape_with_provider(required_super)
            else {
                return Ok(None);
            };
            let spi_pin = ctx.pin_native_root(engine);
            let (ver, coverage) = find(provider).unwrap_or((25.0, USER_PROVIDER_COVERAGE));
            let prov_obj = make_provider(ctx, provider, ver, coverage)?;
            let prov_pin = ctx.pin_native_root(prov_obj);
            let algo_str = ctx.create_string(algo);
            let spi = ctx.read_native_pin(spi_pin, engine);
            let prov_obj = ctx.read_native_pin(prov_pin, prov_obj);
            ctx.unpin_native_roots(spi_pin);
            ctx.unpin_native_roots(prov_pin);
            // `Delegate`'s constructor is PRIVATE; `Delegate.of(spi, algo,
            // provider)` is the JDK's own entry point and is what
            // `MessageDigest.getInstance` itself calls. Going through it also
            // picks `CloneableDelegate` when the SPI is `Cloneable`, which
            // closes this fix's first recorded residual (constructing
            // `Delegate` directly gave a digest whose `clone()` threw where
            // HotSpot clones).
            //
            // The direct constructor is kept as a fallback for images whose
            // `Delegate` has no `of` — it was the shape this code shipped with,
            // and it is correct for any non-`Cloneable` SPI.
            let via_factory = delegate_class
                .rsplit('/')
                .next()
                .is_some_and(|n| n.ends_with("Delegate"))
                .then(|| {
                    ctx.invoke(
                        delegate_class,
                        "of",
                        &format!("{}L{delegate_class};", delegate_desc.trim_end_matches('V')),
                        &[
                            Value::Object(Some(spi)),
                            Value::Object(Some(algo_str)),
                            Value::Object(Some(prov_obj)),
                        ],
                    )
                });
            let built = match via_factory {
                Some(Ok(Some(Value::Object(Some(o))))) => Some(o),
                _ => match ctx.new_object_initialized(
                    delegate_class,
                    delegate_desc,
                    &[
                        Value::Object(Some(spi)),
                        Value::Object(Some(algo_str)),
                        Value::Object(Some(prov_obj)),
                    ],
                ) {
                    Ok(Some(Value::Object(Some(o)))) => Some(o),
                    _ => None,
                },
            };
            match built {
                Some(o) => o,
                // A silent `Ok(None)` here is indistinguishable from "this
                // provider registers nothing", which is the very shape this
                // fix was written to remove — one level further down. Say so.
                None => {
                    tracing::warn!(
                        delegate_class,
                        delegate_desc,
                        provider,
                        type_str,
                        algo,
                        "could not wrap a bare SPI in its engine's Delegate; the caller \
                         will report the algorithm as absent for this provider, which \
                         is NOT the same thing. jca::provider_chain::build_third_party_engine"
                    );
                    return Ok(None);
                }
            }
        }
    };
    // The algorithm name is the PROVIDER's when the provider's own object is
    // what we are handing back. Every engine's `getInstance` in the JDK is
    // written the same way — `MessageDigest`, `KeyPairGenerator`,
    // `Signature`, `KeyFactory`:
    //
    //     if (instance.impl instanceof MessageDigest md) {
    //         md = messageDigest; md.provider = instance.provider;   // NOT .algorithm
    //     } else {
    //         md = Delegate.of((MessageDigestSpi) instance.impl, algorithm, ...);
    //     }
    //
    // Only the WRAPPING arm takes the caller's spelling, and there it goes
    // through the `Delegate` constructor, which has already run above.
    // Overwriting it unconditionally destroyed the provider's own canonical
    // name, and providers publish that name deliberately: BouncyCastle's PQC
    // generators are constructed as
    // `super(Strings.toUpperCase(falconParameters.getName()))` and then quote
    // `getAlgorithm()` back in their own exception text, so
    // `KeyPairGenerator.getInstance("falcon-512", "BC")` refused a mismatched
    // spec with `key pair generator locked to falcon-512` where HotSpot says
    // `FALCON-512` (`pqc.jcajce.provider.test.FalconTest
    // .testRestrictedKeyPairGen`, which asserts the exact string).
    //
    // The fill-in for a null/empty field stays: a provider that never set one
    // would otherwise leave `getAlgorithm()` answering null, and the caller's
    // requested name is a better answer than nothing.
    let needs_algorithm = wrapped_a_bare_spi
        || match ctx.get_field_by_name(engine, "algorithm") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default().is_empty(),
            _ => true,
        };
    if needs_algorithm {
        let pin = ctx.pin_native_root(engine);
        let algo_str = ctx.create_string(algo);
        let engine = ctx.read_native_pin(pin, engine);
        ctx.unpin_native_roots(pin);
        ctx.set_field_by_name(engine, "algorithm", Value::Object(Some(algo_str)));
    }
    record_requested_provider(ctx, engine, provider);
    Ok(Some(engine))
}

/// A GENUINE `javax.crypto.SecretKeyFactory` wrapping `provider`'s own
/// `SecretKeyFactorySpi` for `algo`, built through the JDK's own
/// `(SecretKeyFactorySpi, Provider, String)` constructor — the same one
/// `SecretKeyFactory.getInstance` uses.
///
/// `Ok(None)` means the provider registers nothing for the name; the caller's
/// own refusal stands. `Err` means the provider owns the name but its class
/// would not construct, which is a different fact and must not be hidden behind
/// a fallback.
///
/// Building the REAL wrapper (rather than a synthetic with a delegation flag) is
/// what makes `getKeySpec`/`translateKey` work: this crate intercepts neither,
/// so they run ordinary JDK bytecode, which needs a properly-constructed
/// receiver — `spi`, `algorithm`, `provider` and the inline-initialised `lock`
/// all set by that constructor. The three natives this crate DOES register on
/// the class recognise a receiver they did not build (`skf_receiver_is_ours`)
/// and forward to its `spi`.
pub(crate) fn build_real_secret_key_factory(
    ctx: &mut dyn NativeContext,
    provider: &str,
    algo: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    build_real_spi_wrapper(
        ctx,
        provider,
        "SecretKeyFactory",
        algo,
        algo,
        "javax/crypto/SecretKeyFactory",
        "(Ljavax/crypto/SecretKeyFactorySpi;Ljava/security/Provider;Ljava/lang/String;)V",
    )
}

/// The `KeyFactory` twin of [`build_real_secret_key_factory`], through
/// `java.security.KeyFactory`'s own `(KeyFactorySpi, Provider, String)`
/// constructor. `announce_algo` is the spelling `getAlgorithm()` will echo —
/// the caller's, not the canonical one, which is what HotSpot does.
pub(crate) fn build_real_key_factory(
    ctx: &mut dyn NativeContext,
    provider: &str,
    announce_algo: &str,
    algo: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    build_real_spi_wrapper(
        ctx,
        provider,
        "KeyFactory",
        algo,
        announce_algo,
        "java/security/KeyFactory",
        "(Ljava/security/KeyFactorySpi;Ljava/security/Provider;Ljava/lang/String;)V",
    )
}

/// The `KeyGenerator` twin, through `javax.crypto.KeyGenerator`'s own
/// `(KeyGeneratorSpi, Provider, String)` constructor.
///
/// Reached only from `keygen_get_instance_named`'s refusal path — the same
/// ordering every caller of `build_real_spi_wrapper` obeys, and the whole
/// safety argument for admitting a JDK provider here (see `jdk_service_class`).
///
/// What comes back is a REAL `KeyGenerator` whose `spi` field holds the
/// platform's own generator, not this crate's two-field synthetic. That is the
/// point: the five `SunTls*` KDFs take `TlsKeyMaterialParameterSpec`-family
/// specs that the synthetic's `init` surface cannot carry, and the JDK's
/// generators already implement them. The cost is that every native registered
/// on `javax/crypto/KeyGenerator` now meets receivers it did not build, which
/// `keygen_real_spi` is the guard for.
pub(crate) fn build_real_key_generator(
    ctx: &mut dyn NativeContext,
    provider: &str,
    algo: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    build_real_spi_wrapper(
        ctx,
        provider,
        "KeyGenerator",
        algo,
        algo,
        "javax/crypto/KeyGenerator",
        "(Ljavax/crypto/KeyGeneratorSpi;Ljava/security/Provider;Ljava/lang/String;)V",
    )
}

/// The `Mac` twin of [`build_real_key_factory`], through
/// `javax.crypto.Mac`'s own `(MacSpi, Provider, String)` constructor.
pub(crate) fn build_real_mac(
    ctx: &mut dyn NativeContext,
    provider: &str,
    announce_algo: &str,
    algo: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    build_real_spi_wrapper(
        ctx,
        provider,
        "Mac",
        algo,
        announce_algo,
        "javax/crypto/Mac",
        "(Ljavax/crypto/MacSpi;Ljava/security/Provider;Ljava/lang/String;)V",
    )
}

/// Shared body: instantiate `provider`'s registered SPI class for
/// `(type_str, algo)` and hand it to `engine_class`'s own
/// `(Spi, Provider, String)` constructor.
fn build_real_spi_wrapper(
    ctx: &mut dyn NativeContext,
    provider: &str,
    type_str: &str,
    algo: &str,
    announce_algo: &str,
    engine_class: &str,
    ctor_desc: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // A JDK provider is admitted here too, and ONLY because every caller of
    // this function is already on its engine's refusal path — see
    // `jdk_service_class` for why that ordering is the whole safety argument.
    if third_party_service_class(Some(provider), type_str, algo).is_none()
        && jdk_service_class(Some(provider), type_str, algo).is_none()
    {
        return Ok(None);
    }
    let Some(impl_result) = build_jca_impl(ctx, provider, type_str, algo) else {
        return Ok(None);
    };
    let spi = match impl_result? {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let spi_pin = ctx.pin_native_root(spi);
    let (ver, coverage) = find(provider).unwrap_or((25.0, USER_PROVIDER_COVERAGE));
    let prov_obj = make_provider(ctx, provider, ver, coverage)?;
    let prov_pin = ctx.pin_native_root(prov_obj);
    let algo_str = ctx.create_string(announce_algo);
    let spi = ctx.read_native_pin(spi_pin, spi);
    let prov_obj = ctx.read_native_pin(prov_pin, prov_obj);
    ctx.unpin_native_roots(spi_pin);
    ctx.unpin_native_roots(prov_pin);
    let built = ctx.new_object_initialized(
        engine_class,
        ctor_desc,
        &[
            Value::Object(Some(spi)),
            Value::Object(Some(prov_obj)),
            Value::Object(Some(algo_str)),
        ],
    )?;
    match built {
        Some(Value::Object(Some(o))) => Ok(Some(o)),
        _ => Ok(None),
    }
}

/// Crack a legacy-style put key (e.g. `"Cipher.AES/GCM/NoPadding"`,
/// `"Alg.Alias.Cipher.AES"`, `"Cipher AES/GCM/NoPadding SupportedModes"`)
/// into `(kind, type, algorithm, attribute_or_alias_target)`.
///
/// Returns `None` for keys that aren't service-shaped (e.g. arbitrary
/// `Properties` reads BC stashes inside its provider — those are
/// stored in the property bag but ignored by service resolution).
///
/// `kind` is one of:
///   - `"primary"` — `"Type.Algorithm"` registers a Service entry.
///   - `"alias"`   — `"Alg.Alias.Type.Alias"` maps Alias → value.
///   - `"attr"`    — `"Type.Algorithm AttrName"` sets an attribute
///                   on an existing Service.  We currently store these
///                   in the entry's class_name slot only when they are
///                   the recognized `ImplementedIn` style; the attribute
///                   path is mostly cosmetic for resolution.
///
/// Mirrors OpenJDK's `Provider.parseLegacyPut(String, String)` —
/// see https://github.com/openjdk/jdk/blob/jdk-25%2B/src/java.base/share/classes/java/security/Provider.java.
fn parse_legacy_key(key: &str) -> Option<(String, String, String, Option<String>)> {
    // Attribute form: "Type.Algorithm AttrName" — split on first space.
    let (head, attr) = match key.find(' ') {
        Some(idx) => (&key[..idx], Some(key[idx + 1..].trim().to_string())),
        None => (key, None),
    };
    let head = head.trim();
    if head.is_empty() {
        return None;
    }

    // Alias form: "Alg.Alias.<Type>.<Alias>" — three dots minimum.
    if let Some(rest) = head.strip_prefix("Alg.Alias.") {
        let dot = rest.find('.')?;
        let type_str = &rest[..dot];
        let alias = &rest[dot + 1..];
        if type_str.is_empty() || alias.is_empty() {
            return None;
        }
        return Some((
            "alias".to_string(),
            type_str.to_string(),
            alias.to_string(),
            None,
        ));
    }

    // Primary / attr form: "Type.Algorithm[ AttrName]" — split on FIRST dot.
    let dot = head.find('.')?;
    let type_str = &head[..dot];
    let algorithm = &head[dot + 1..];
    if type_str.is_empty() || algorithm.is_empty() {
        return None;
    }
    let kind = if attr.is_some() { "attr" } else { "primary" };
    Some((
        kind.to_string(),
        type_str.to_string(),
        algorithm.to_string(),
        attr,
    ))
}

/// Internal API: register a single service entry.  Test helper plus
/// the mechanism `provider_put_native` and `provider_parse_legacy_put_native`
/// both funnel through.
fn put_service(provider: &str, type_str: &str, algorithm: &str, value: &str) {
    let type_n = normalize_engine(type_str);
    let algo_n = normalize_algo(algorithm);
    let mut s = services().lock();
    let map = s.entry(provider.to_string()).or_default();
    // A provider may `put` the attribute rows before the primary row (the
    // order inside `Provider.putAll` is a HashMap iteration order), so the
    // primary must not wipe attributes already recorded under the same key.
    let attributes = map
        .get(&(type_n.clone(), algo_n.clone()))
        .map(|e| e.attributes.clone())
        .unwrap_or_default();
    let entry = ServiceEntry {
        type_str: type_str.to_string(),
        algorithm: algorithm.to_string(),
        class_name: value.to_string(),
        key: format!("{type_str}.{algorithm}"),
        attributes,
    };
    map.insert((type_n, algo_n), entry);
}

/// Record a `Type.Algorithm AttrName` legacy row against its service.
///
/// The service itself may not have been `put` yet, so a placeholder entry with
/// an empty class name is created and later filled in by `put_service` — which
/// carries the attributes over.
fn put_service_attribute(provider: &str, type_str: &str, algorithm: &str, attr: &str, value: &str) {
    let type_n = normalize_engine(type_str);
    let algo_n = normalize_algo(algorithm);
    let mut s = services().lock();
    let map = s.entry(provider.to_string()).or_default();
    let entry = map.entry((type_n, algo_n)).or_insert_with(|| ServiceEntry {
        type_str: type_str.to_string(),
        algorithm: algorithm.to_string(),
        class_name: String::new(),
        key: format!("{type_str}.{algorithm}"),
        attributes: Vec::new(),
    });
    if let Some(slot) = entry
        .attributes
        .iter_mut()
        .find(|(name, _)| name.eq_ignore_ascii_case(attr))
    {
        slot.1 = value.to_string();
    } else {
        entry.attributes.push((attr.to_string(), value.to_string()));
    }
}

/// Seed ownership data for the direct-native KeyFactory, Signature,
/// SecureRandom, and Cipher routes. These routes bypass GetInstance but must
/// make the same provider-service decision as the JDK.
fn seed_direct_native_engine_services() {
    const SUN: &str = "SUN";
    // Every name here must be one `message_digest::algorithm_supported`
    // accepts. `MD2` sat in this literal and was refused by `getInstance` for
    // three waves; `SHAKE128-256` / `SHAKE256-512` were the opposite omission,
    // deliberately withheld while unimplemented. Both were closed on
    // 2026-08-12 by implementing the digest and then adding the name, in that
    // order. `every_advertised_sun_message_digest_is_serviceable` is now the
    // ratchet that keeps this array and that predicate one set.
    // W7-63-jca-advertise-vs-serve.md.
    for algorithm in [
        "MD2",
        "MD5",
        "SHA-1",
        "SHA-224",
        "SHA-256",
        "SHA-384",
        "SHA-512",
        "SHA-512/224",
        "SHA-512/256",
        "SHA3-224",
        "SHA3-256",
        "SHA3-384",
        "SHA3-512",
        "SHAKE128-256",
        "SHAKE256-512",
    ] {
        put_service(
            SUN,
            "MessageDigest",
            algorithm,
            "sun.security.provider.Native",
        );
    }
    // SUN's `MessageDigest` ALIASES, enumerated from
    // `Security.getProvider("SUN").keySet()` on HotSpot 25 rather than
    // recalled: forty rows, of which this seed carried three.
    //
    // An alias is not decoration. `check_provider_ownership` gates every
    // NAMED-provider `getInstance` on `get_service_entry`, which resolves the
    // alias table first, so a name HotSpot's SUN answers and this table does
    // not is a refusal against a JDK provider for a digest it demonstrably
    // implements:
    //
    // ```text
    // MessageDigest.getInstance("SHA1", "SUN")
    //   HotSpot 25 -> a working SHA-1
    //   CratonVM   -> NoSuchAlgorithmException:
    //                 no such algorithm: SHA1 for provider SUN
    // ```
    //
    // MEASURED. That is bc-java `cms`'s `SunProviderTest.testSHA1WithRSAStream`,
    // the last CratonVM failure in the 53-class sweep of 2026-08-24, and the
    // un-hyphenated spelling is the one every pre-JCA-4 caller uses. The
    // hyphenated `SHA-1` resolved all along, which is why this survived: the
    // primary name is a service, and only the alias spellings were missing.
    //
    // Every canonical below is already a seeded SUN service (the array above),
    // so this widens the SPELLINGS this provider answers to and not the set of
    // digests it claims — `sun_message_digest_aliases_all_resolve_to_a_seeded_service`
    // is the ratchet on exactly that. Aliases stay out of
    // `Security.getAlgorithms("MessageDigest")` (see `algorithms_for_service`),
    // so the advertised count remains HotSpot's 15.
    for (alias, canonical) in [
        ("SHA", "SHA-1"),
        ("SHA1", "SHA-1"),
        ("SHA224", "SHA-224"),
        ("SHA256", "SHA-256"),
        ("SHA384", "SHA-384"),
        ("SHA512", "SHA-512"),
        ("SHA512/224", "SHA-512/224"),
        ("SHA512/256", "SHA-512/256"),
    ] {
        put_alias(SUN, "MessageDigest", alias, canonical);
    }
    // The OID rows, in both spellings HotSpot registers: bare, and `OID.`-
    // prefixed. X.509/PKCS/CMS callers name digests this way — bc-java's
    // `EnvelopedDataHelper` and `DigestFactory` among them — and
    // `canonical_if_unrecognised` needs the row to map the OID onto a name the
    // engine's own table knows.
    for (oid, canonical) in [
        ("1.2.840.113549.2.2", "MD2"),
        ("1.2.840.113549.2.5", "MD5"),
        ("1.3.14.3.2.26", "SHA-1"),
        ("2.16.840.1.101.3.4.2.1", "SHA-256"),
        ("2.16.840.1.101.3.4.2.2", "SHA-384"),
        ("2.16.840.1.101.3.4.2.3", "SHA-512"),
        ("2.16.840.1.101.3.4.2.4", "SHA-224"),
        ("2.16.840.1.101.3.4.2.5", "SHA-512/224"),
        ("2.16.840.1.101.3.4.2.6", "SHA-512/256"),
        ("2.16.840.1.101.3.4.2.7", "SHA3-224"),
        ("2.16.840.1.101.3.4.2.8", "SHA3-256"),
        ("2.16.840.1.101.3.4.2.9", "SHA3-384"),
        ("2.16.840.1.101.3.4.2.10", "SHA3-512"),
        ("2.16.840.1.101.3.4.2.11", "SHAKE128-256"),
        ("2.16.840.1.101.3.4.2.12", "SHAKE256-512"),
    ] {
        put_alias(SUN, "MessageDigest", oid, canonical);
        put_alias(SUN, "MessageDigest", &format!("OID.{oid}"), canonical);
    }
    // ALIASES, not services. HotSpot's SUN carries
    // `Alg.Alias.MessageDigest.SHAKE128 = SHAKE128-256`, so
    // `MessageDigest.getInstance("SHAKE128")` resolves and returns bytes
    // identical to the hyphenated primary (measured, §C of
    // probes/JcaAdvertisedVsServedProbe.expected.txt) while
    // `Security.getAlgorithms("MessageDigest")` does NOT list it — aliases are
    // excluded from that walk, see `algorithms_for_service`. Registering these
    // as services instead would make the advertised set 17 where HotSpot
    // answers 15.
    put_alias(SUN, "MessageDigest", "SHAKE128", "SHAKE128-256");
    put_alias(SUN, "MessageDigest", "SHAKE256", "SHAKE256-512");
    // `ML-DSA` — the UMBRELLA name — was deliberately absent from this
    // `KeyFactory` list until 2026-08-14, while the three parameter-set names
    // stayed. It is back because the ALTERNATIVE the earlier record declined
    // has now been taken: `kf_algo_idx` carries an umbrella arm
    // (`ALGO_MLDSA_GENERIC`) and `pqc_umbrella_keyfactory_class` drives the
    // JDK's own non-nested `sun.security.provider.ML_DSA_Impls$KF`, which is
    // the factory that resolves the parameter set from the key's encoding.
    //
    // The reason the record gave for declining was not "too much work": it had
    // RUN the three names that already resolved and found the objects partly
    // unusable one accessor in —
    // `KeyFactory.getInstance("ML-DSA-44").getProvider()` raised
    // `NullPointerException: Cannot enter synchronized block because
    // "this.lock" is null`, where HotSpot answers `SUN version 25`. Widening a
    // surface that is already broken is not a fix, and that objection is
    // ANSWERED rather than ignored: `kf_get_provider` is registered in the
    // same change, so every one of these names now reports its provider
    // instead of throwing from a plain accessor.
    // W7-63-jca-advertise-vs-serve.md.
    for algorithm in ["DSA", "ML-DSA", "ML-DSA-44", "ML-DSA-65", "ML-DSA-87"] {
        put_service(SUN, "KeyFactory", algorithm, "sun.security.provider.Native");
    }
    for algorithm in ["DRBG", "SHA1PRNG"] {
        put_service(
            SUN,
            "SecureRandom",
            algorithm,
            "sun.security.provider.SecureRandom",
        );
    }
    for algorithm in ["ML-DSA", "ML-DSA-44", "ML-DSA-65", "ML-DSA-87"] {
        put_service(SUN, "Signature", algorithm, "sun.security.provider.Native");
    }
    // The WHOLE `sun.security.provider.DSA` family — twenty names, of which
    // this list carried two.
    //
    // The eighteen missing ones are not a different kind of thing from the two
    // that were here: this engine computes NO DSA signature itself
    // (`crypto_impl` has no DSA arm at all), so `SHA1withDSA` was already the
    // JDK's own SPI driven from `signature::drive_real_signature_spi`, and the
    // other eighteen are the same drive against a sibling class that
    // `dsa_family_spi_class` derives from the caller's own spelling. Nine of
    // them are the `inP1363Format` twins, which are NOT a formatting flag this
    // engine could apply — the JDK implements the IEEE P1363 fixed-width
    // `r || s` encoding by subclassing, so routing to the class is what makes
    // the format right too.
    //
    // The class name is the REAL one per row, not the `.Native` marker: this is
    // a family the JDK actually implements for us, and `getServices()` reports
    // `getClassName()`, so a marker here would leave twenty rows differing from
    // HotSpot's enumeration for no reason.
    for algorithm in crate::jca::signature::DSA_FAMILY_SIGNATURE_NAMES {
        let cls = crate::jca::signature::dsa_family_service_class(algorithm)
            .expect("DSA_FAMILY_SIGNATURE_NAMES is exactly dsa_family_spi_class's domain");
        put_service(SUN, "Signature", algorithm, &cls);
    }
    // `HSS/LMS` (RFC 8554), which SUN has carried since JDK 21 and this list
    // never had. Both halves are the platform's own classes: the `KeyFactory`
    // is reached because `kf_get_instance` falls to `build_real_key_factory`
    // for a name `kf_algo_idx` refuses, and the `Signature` through
    // `signature::dsa_real_spi_class`'s `SIG_HSS_LMS` arm.
    put_service(SUN, "KeyFactory", "HSS/LMS", "sun.security.provider.HSS$KeyFactoryImpl");
    put_service(SUN, "Signature", "HSS/LMS", "sun.security.provider.HSS");
    // `Configuration.JavaLoginConfig`, the JAAS login-configuration provider.
    // `javax.security.auth.login.Configuration.getInstance` is not intercepted
    // by this crate, so the row IS the implementation; without it
    // `getInstance("JavaLoginConfig", null)` refused on a VM carrying a working
    // `ConfigFile$Spi`. `JcaResolveAll` reports this engine as a SKIP — its API
    // is not the `getInstance(String)` shape that probe models — so the gap
    // census could say nothing about it; `apps/probes/JcaModernEngines` asks it
    // directly.
    put_service(
        SUN,
        "Configuration",
        "JavaLoginConfig",
        "sun.security.provider.ConfigFile$Spi",
    );
    // `DSA` and `DSS` are ALIASES of `SHA1withDSA` on HotSpot, not services.
    // Seeding `DSA` as a primary made `Security.getAlgorithms("Signature")`
    // report a name HotSpot does not — measured 2026-09-02, it was the only
    // row this VM advertised and HotSpot did not. `getInstance("DSA")` is
    // unaffected: `algo_idx` maps it directly, and the alias table resolves it
    // for the named-provider gate.
    put_alias(SUN, "Signature", "DSA", "SHA1withDSA");
    put_alias(SUN, "Signature", "DSS", "SHA1withDSA");

    const RSA: &str = "SunRsaSign";
    for algorithm in ["RSA", "RSASSA-PSS"] {
        put_service(
            RSA,
            "KeyFactory",
            algorithm,
            "sun.security.rsa.RSAKeyFactory",
        );
    }
    // `AlgorithmParameters.RSASSA-PSS` — the PSS parameter block
    // (`sun.security.rsa.PSSParameters`). `AlgorithmParameters.getInstance` is
    // not intercepted by this crate, so the row is the implementation; without
    // it, a caller decoding a PSS `AlgorithmIdentifier` — which is how every
    // X.509 PSS certificate carries its salt length and MGF — got
    // `NoSuchAlgorithmException` from `AlgorithmId.decodeParams()`.
    put_service(
        RSA,
        "AlgorithmParameters",
        "RSASSA-PSS",
        "sun.security.rsa.PSSParameters",
    );
    for algorithm in [
        "MD2withRSA",
        "MD5withRSA",
        "SHA1withRSA",
        "SHA224withRSA",
        "SHA256withRSA",
        "SHA384withRSA",
        "SHA512withRSA",
        "SHA512/224withRSA",
        "SHA512/256withRSA",
        "SHA3-224withRSA",
        "SHA3-256withRSA",
        "SHA3-384withRSA",
        "SHA3-512withRSA",
        "RSASSA-PSS",
    ] {
        put_service(RSA, "Signature", algorithm, "sun.security.rsa.RSASignature");
    }

    seed_sunec_services();
    for algorithm in ["Ed25519", "Ed448", "EdDSA", "X25519", "X448", "XDH"] {
        put_service("SunEC", "KeyFactory", algorithm, "sun.security.ec.Native");
    }
    for algorithm in ["Ed25519", "Ed448", "EdDSA"] {
        put_service("SunEC", "Signature", algorithm, "sun.security.ec.Native");
    }

    const JCE: &str = "SunJCE";
    // The generic Cipher.AES service owns the AES transformations whose mode
    // lives in an attribute rather than the service name (ECB/CBC/CFB/OFB); the
    // fully-spelled names below are separate services on HotSpot and are
    // separate services here. `jca::cipher::classify_transformation` decides
    // which mode/padding combinations each one really admits, and this list is
    // kept equal to what that function can COMPUTE — advertised and implemented
    // are one set, walked in both directions.
    //
    // Four names came OUT of this list in the W7-15 lane, each measured against
    // jdk-25.0.3.9-hotspot before removal — and all four went back IN on
    // 2026-08-11 once they were implemented. Removing them was the correct
    // answer to "advertised but not computable"; it was never the preferred
    // one, and the preferred one is now available:
    //
    //   * `ChaCha20` and `ChaCha20-Poly1305` — no ChaCha20 implementation was
    //     reachable from `Cipher` at all. Advertising them was not an
    //     aspiration, it was wrong crypto: the engine's mode-only dispatch
    //     turned both into AES-256-ECB, byte-identically to
    //     `AES/ECB/PKCS5Padding`, discarding the nonce and (for the AEAD name)
    //     producing no tag, so a tampered ciphertext decrypted without an
    //     authentication failure. An AEAD that cannot fail on a bad tag is
    //     worse than no AEAD, because callers build integrity guarantees on it.
    //     **RFC 8439 is now implemented in `native-builtins/src/chacha20.rs`**,
    //     pinned by the RFC's own vectors and a differential test against the
    //     `chacha20poly1305` crate, and `regression-suite/src/RChaCha20Cipher.java`
    //     matches HotSpot byte-for-byte in both modes.
    //   * `AES/KW/PKCS5Padding` and `AES/KWP/NoPadding` — no path implemented
    //     either. KWP is RFC 5649, a different padded-wrap scheme with its own
    //     ICV and length prefix, not RFC 3394 with a padding bolted on;
    //     `aes_key_wrap` computed RFC 3394 only. **Both are now implemented**
    //     (`aes_key_wrap_with_padding` for RFC 5649, PKCS#5 at an eight-byte
    //     block size for the other), against SunJCE's own wrap vectors.
    //
    // What went IN is the other direction of the same census — code that works
    // and was never advertised, which is the quieter half of the same defect:
    // `DES`/`DESede` (computed through the real SunJCE SPI; measured
    // byte-identical to HotSpot for `DESede/CBC/PKCS5Padding`), the two
    // `PBEWithHmacSHA224AndAES_*` names `jca::cipher::pbes2_aes_params` has
    // always derived, and the size-pinned `AES_128/192/256` transformations,
    // which now enforce their key length at `init`.
    for algorithm in [
        "AES",
        "AES/GCM/NoPadding",
        "AES/KW/NoPadding",
        "AES/KW/PKCS5Padding",
        "AES/KWP/NoPadding",
        // W7-39, 2026-08-12. Both were removed from this list on 2026-08-11
        // while `Cipher.getInstance` served them as AES-128-ECB — the right
        // move then, because a wrong cipher is worse than a missing one. They
        // are back because they are now COMPUTED, by the real SunJCE
        // `BlowfishCipher` / `ARCFOURCipher` SPI (`jca::cipher::
        // drive_real_ecb_cipher`), byte-identically to HotSpot 25. `ARCFOUR` is
        // the service and `RC4` its alias, which is HotSpot's own arrangement
        // and the reason `Security.getAlgorithms("Cipher")` names only one.
        //
        // RC4 is broken cryptography and nothing here recommends it. It is
        // advertised for the same reason DES is: a workload that asks for it on
        // HotSpot gets bytes, and on this VM it got a hard failure. Refusing an
        // algorithm the platform implements is a portability defect, not a
        // security control — a caller who must not use RC4 is not stopped by
        // this VM lacking it.
        "ARCFOUR",
        "Blowfish",
        "ChaCha20",
        "ChaCha20-Poly1305",
        // The bare names, as HotSpot lists them, carrying their mode set in a
        // `SupportedModes` attribute rather than in the service name.
        //
        // These were spelled in full until 2026-08-27, deliberately, and the
        // reason was recorded here: this engine routed only CBC to the real
        // SunJCE SPI and the bare name defaults to ECB, so advertising
        // `DESede` would have named a transformation `getInstance` refuses.
        // That precondition is gone — `classify_transformation` admits ECB for
        // this family and `cipher_do_final_impl` forwards the parsed mode — so
        // the bare form is now both advertised and computed, which is the
        // invariant `every_advertised_sunjce_cipher_is_serviceable` pins and
        // the shape HotSpot actually has.
        //
        // The fully-spelled `DESede/CBC/PKCS5Padding` is still SERVED: the
        // `Cipher` arm of `check_provider_ownership` splits a transformation on
        // `/` and asks about the base name, so one bare service covers every
        // admitted mode/padding of it. It is no longer separately ADVERTISED,
        // which is what `Security.getAlgorithms("Cipher")` reports, and there
        // HotSpot lists two names where this list used to produce four.
        "DES",
        "DESede",
        "RSA",
        "PBEWithHmacSHA1AndAES_128",
        "PBEWithHmacSHA1AndAES_256",
        "PBEWithHmacSHA224AndAES_128",
        "PBEWithHmacSHA224AndAES_256",
        "PBEWithHmacSHA256AndAES_128",
        "PBEWithHmacSHA256AndAES_256",
    ] {
        put_service(JCE, "Cipher", algorithm, "com.sun.crypto.provider.Native");
    }
    // The size-pinned family. HotSpot registers the same shape — one service
    // per (size, mode) with `NoPadding` spelled in the name, and NO bare
    // `AES_128` service (measured: `Cipher.getInstance("AES_128")` raises while
    // `AES_128/CBC/NoPadding` resolves). `KWP` and `KW/PKCS5Padding` are in
    // HotSpot's set and deliberately absent from ours, for the reason above.
    for size in ["AES_128", "AES_192", "AES_256"] {
        for mode in ["CBC", "CFB", "ECB", "GCM", "KW", "OFB"] {
            put_service(
                JCE,
                "Cipher",
                &format!("{size}/{mode}/NoPadding"),
                "com.sun.crypto.provider.Native",
            );
        }
        // `KW/PKCS5Padding` and `KWP/NoPadding`, with the JDK's REAL class
        // names — and the class name is the whole difference between this
        // working and not.
        //
        // `jca-provider-population-gap-20260830.md` §5.2 ran this experiment
        // ("add six `put_service` rows, rebuild, ask"), got
        // `NoSuchAlgorithmException` unchanged, and concluded "a service row is
        // not sufficient even for the one engine that walks the chain". The
        // conclusion is right about a row carrying the `.Native` MARKER, which
        // is what the rest of this loop seeds: `try_delegate_cipher_to_chain`
        // finds such a row, asks `build_jca_impl` to instantiate
        // `com.sun.crypto.provider.Native`, gets a class-not-found, and moves
        // on to the next provider — ending at the caller's original refusal,
        // which is exactly what §5.2 measured.
        //
        // A row naming `KeyWrapCipher$AES128_KW_PKCS5Padding` instantiates. The
        // marker means "a Rust engine answers this"; these six have no Rust
        // engine and are answered by the platform's own class, which is why
        // they are the only rows in this loop that carry a real name.
        for (mode, padding) in [("KW", "PKCS5Padding"), ("KWP", "NoPadding")] {
            let bits = size.trim_start_matches("AES_");
            put_service(
                JCE,
                "Cipher",
                &format!("{size}/{mode}/{padding}"),
                &format!(
                    "com.sun.crypto.provider.KeyWrapCipher$AES{bits}_{mode}_{padding}"
                ),
            );
        }
    }
    // The `ML-KEM` UMBRELLA was absent for the same reason `ML-DSA` was absent
    // from the `SUN` `KeyFactory` list above, and it was found by the census
    // rather than by either record. Both are back on 2026-08-14 and for the
    // same reason: `kf_algo_idx` carries the umbrella arm and
    // `pqc_umbrella_keyfactory_class` drives the JDK's own non-nested
    // `ML_KEM_Impls$KF`. Note that the SPI class name in a service row was
    // never evidence of anything — `kf_get_instance` intercepts natively and
    // never reaches `build_jca_impl` — so what makes these rows truthful is
    // the engine arm, not the string.
    // W7-63-jca-advertise-vs-serve.md.
    for algorithm in ["ML-KEM", "ML-KEM-512", "ML-KEM-768", "ML-KEM-1024"] {
        put_service(
            JCE,
            "KeyFactory",
            algorithm,
            "com.sun.crypto.provider.ML_KEM_Impls$KF",
        );
    }
    // Finite-field Diffie-Hellman, registered under the name HotSpot uses.
    // `DH` is an ALIAS there, not a service, which is why the advertised list
    // names `DiffieHellman` while every caller types `DH`.
    put_service(
        JCE,
        "KeyFactory",
        "DiffieHellman",
        "com.sun.crypto.provider.DHKeyFactory",
    );
    put_alias(JCE, "KeyFactory", "DH", "DiffieHellman");
    // SunJCE's own aliases. Aliases are excluded from
    // `Security.getAlgorithms` (their property key is `Alg.Alias.Cipher.X`, not
    // `Cipher.X`), so these widen `getInstance`/`getService` resolution without
    // lengthening the advertised list — which is exactly their role on HotSpot.
    put_alias(JCE, "Cipher", "AESWrap", "AES/KW/NoPadding");
    put_alias(JCE, "Cipher", "AESWrap_128", "AES_128/KW/NoPadding");
    put_alias(JCE, "Cipher", "AESWrap_192", "AES_192/KW/NoPadding");
    put_alias(JCE, "Cipher", "AESWrap_256", "AES_256/KW/NoPadding");
    // `Alg.Alias.Cipher.TripleDES = DESede` on SunJCE — the BARE name,
    // measured, not the expanded transformation this row used to carry.
    //
    // The expansion was wrong crypto, not merely a wrong spelling: bare
    // `DESede` is ECB/PKCS5Padding on SunJCE and this row named CBC, so
    // once the alias registry started being consulted by
    // `canonical_transformation` (2026-08-26), `getInstance("TripleDES")`
    // returned a CBC cipher with a random IV where HotSpot returns ECB
    // with none — different ciphertext, silently. That is exactly the
    // mode substitution `classify_transformation`'s DesFamily arm and
    // `cipher_do_final_impl`'s route table each carry a comment about;
    // the alias reached the engine around both of them.
    put_alias(JCE, "Cipher", "TripleDES", "DESede");
    // `Alg.Alias.Cipher.RC4 = ARCFOUR` on SunJCE. Measured on HotSpot 25: both
    // spellings resolve, both answer `getProvider()=SunJCE`, and both encrypt
    // the same 16-byte plaintext to `27ca482b161e3ab93f812659b904df95` — while
    // `Security.getAlgorithms("Cipher")` lists ARCFOUR alone.
    put_alias(JCE, "Cipher", "RC4", "ARCFOUR");

    // Windows only — the provider itself is absent from the seed chain on
    // every other platform (see `provider_chain`), and seeding its services
    // anyway would leave `Security.getProvider("SunMSCAPI")` answering a live
    // 16-service Provider for a name no image on this host declares.
    if cfg!(target_os = "windows") {
        const MSCAPI: &str = "SunMSCAPI";
        for algorithm in ["RSA", "RSA/ECB/PKCS1Padding"] {
            put_service(
                MSCAPI,
                "Cipher",
                algorithm,
                "sun.security.mscapi.CRSACipher",
            );
        }
        put_service(
            MSCAPI,
            "SecureRandom",
            "Windows-PRNG",
            "sun.security.mscapi.PRNG",
        );
        for algorithm in [
            "MD2withRSA",
            "MD5withRSA",
            "NONEwithRSA",
            "RSASSA-PSS",
            "SHA1withRSA",
            "SHA256withRSA",
            "SHA384withRSA",
            "SHA512withRSA",
            "SHA1withECDSA",
            "SHA224withECDSA",
            "SHA256withECDSA",
            "SHA384withECDSA",
            "SHA512withECDSA",
        ] {
            put_service(
                MSCAPI,
                "Signature",
                algorithm,
                "sun.security.mscapi.CSignature",
            );
        }
    }
    put_service(
        "SunJSSE",
        "Signature",
        "MD5andSHA1withRSA",
        "sun.security.ssl.RSASignature",
    );
    seed_measured_jdk25_provider_aliases();
    seed_retired_getalgorithms_literals();
}

/// Every `Alg.Alias.<Type>.<Alias>` row a JDK 25 provider declares that this VM
/// can actually honour.
///
/// ENUMERATED, not recalled: `Security.getProvider(p).keySet()` on HotSpot 25
/// for SUN, SunJCE, SunRsaSign, SunEC and SunJSSE gives 401 alias rows. Each
/// was then replayed through `<Type>.getInstance(name, provider)` on BOTH VMs,
/// and the 401 split three ways:
///
///   * 67 this VM already answered;
///   * 70 whose CANONICAL this VM does not serve either — seeding those would
///     trade a refusal at `getInstance` for a failure one call later, so they
///     are deliberately absent and stay `NoSuchAlgorithmException`;
///   * 264 where the canonical resolves and only the SPELLING was refused.
///
/// This is that 264. The filter is the whole point: an alias may only be
/// declared for a name this VM can already produce an engine for, which is
/// what `every_measured_alias_resolves_to_a_serviceable_canonical` re-checks.
///
/// The rows are overwhelmingly OIDs, and that is who needs them: every
/// X.509/PKCS/CMS caller names algorithms by OID, and an OID IS an alias row.
/// `check_provider_ownership` reads `get_service_entry`, which resolves this
/// table before the service map, so a missing row is a refusal against a JDK
/// provider for an algorithm it demonstrably implements.
///
/// Both OID spellings are listed where HotSpot lists both. It does not always:
/// nine legacy OIDs (`1.3.14.3.2.12` and friends, `1.2.840.113549.1.1.1`)
/// appear bare with no `OID.`-prefixed twin, so the spellings are transcribed
/// rather than generated — matching the oracle includes matching where the
/// oracle is irregular.
///
/// Seeding a row is necessary and not always sufficient: an engine that gates
/// on its own hand-written name table still has to resolve the alias before it
/// looks, which is what `canonical_if_unrecognised` is for. The engines that
/// call it serve these; the ones that do not are named in the record.
const MEASURED_JDK25_PROVIDER_ALIASES: &[(&str, &str, &str, &str)] = &[
    // SUN AlgorithmParameters -- 2 rows
    ("SUN", "AlgorithmParameters", "1.3.14.3.2.12", "DSA"),
    ("SUN", "AlgorithmParameters", "OID.1.2.840.10040.4.1", "DSA"),
    // SUN KeyFactory -- 9 rows
    ("SUN", "KeyFactory", "1.2.840.10040.4.1", "DSA"),
    ("SUN", "KeyFactory", "1.3.14.3.2.12", "DSA"),
    ("SUN", "KeyFactory", "2.16.840.1.101.3.4.3.17", "ML-DSA-44"),
    ("SUN", "KeyFactory", "2.16.840.1.101.3.4.3.18", "ML-DSA-65"),
    ("SUN", "KeyFactory", "2.16.840.1.101.3.4.3.19", "ML-DSA-87"),
    ("SUN", "KeyFactory", "OID.1.2.840.10040.4.1", "DSA"),
    (
        "SUN",
        "KeyFactory",
        "OID.2.16.840.1.101.3.4.3.17",
        "ML-DSA-44",
    ),
    (
        "SUN",
        "KeyFactory",
        "OID.2.16.840.1.101.3.4.3.18",
        "ML-DSA-65",
    ),
    (
        "SUN",
        "KeyFactory",
        "OID.2.16.840.1.101.3.4.3.19",
        "ML-DSA-87",
    ),
    // SUN KeyPairGenerator -- 9 rows
    ("SUN", "KeyPairGenerator", "1.2.840.10040.4.1", "DSA"),
    ("SUN", "KeyPairGenerator", "1.3.14.3.2.12", "DSA"),
    (
        "SUN",
        "KeyPairGenerator",
        "2.16.840.1.101.3.4.3.17",
        "ML-DSA-44",
    ),
    (
        "SUN",
        "KeyPairGenerator",
        "2.16.840.1.101.3.4.3.18",
        "ML-DSA-65",
    ),
    (
        "SUN",
        "KeyPairGenerator",
        "2.16.840.1.101.3.4.3.19",
        "ML-DSA-87",
    ),
    ("SUN", "KeyPairGenerator", "OID.1.2.840.10040.4.1", "DSA"),
    (
        "SUN",
        "KeyPairGenerator",
        "OID.2.16.840.1.101.3.4.3.17",
        "ML-DSA-44",
    ),
    (
        "SUN",
        "KeyPairGenerator",
        "OID.2.16.840.1.101.3.4.3.18",
        "ML-DSA-65",
    ),
    (
        "SUN",
        "KeyPairGenerator",
        "OID.2.16.840.1.101.3.4.3.19",
        "ML-DSA-87",
    ),
    // SUN Signature -- 17 rows
    ("SUN", "Signature", "1.2.840.10040.4.3", "SHA1withDSA"),
    ("SUN", "Signature", "1.3.14.3.2.13", "SHA1withDSA"),
    ("SUN", "Signature", "1.3.14.3.2.27", "SHA1withDSA"),
    ("SUN", "Signature", "2.16.840.1.101.3.4.3.17", "ML-DSA-44"),
    ("SUN", "Signature", "2.16.840.1.101.3.4.3.18", "ML-DSA-65"),
    ("SUN", "Signature", "2.16.840.1.101.3.4.3.19", "ML-DSA-87"),
    (
        "SUN",
        "Signature",
        "2.16.840.1.101.3.4.3.2",
        "SHA256withDSA",
    ),
    ("SUN", "Signature", "DSAWithSHA1", "SHA1withDSA"),
    ("SUN", "Signature", "OID.1.2.840.10040.4.3", "SHA1withDSA"),
    (
        "SUN",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.17",
        "ML-DSA-44",
    ),
    (
        "SUN",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.18",
        "ML-DSA-65",
    ),
    (
        "SUN",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.19",
        "ML-DSA-87",
    ),
    (
        "SUN",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.2",
        "SHA256withDSA",
    ),
    ("SUN", "Signature", "SHA-1/DSA", "SHA1withDSA"),
    ("SUN", "Signature", "SHA/DSA", "SHA1withDSA"),
    ("SUN", "Signature", "SHA1/DSA", "SHA1withDSA"),
    ("SUN", "Signature", "SHAwithDSA", "SHA1withDSA"),
    // SunEC AlgorithmParameters -- 1 rows
    (
        "SunEC",
        "AlgorithmParameters",
        "OID.1.2.840.10045.2.1",
        "EC",
    ),
    // SunEC KeyAgreement -- 4 rows
    ("SunEC", "KeyAgreement", "1.3.101.110", "X25519"),
    ("SunEC", "KeyAgreement", "1.3.101.111", "X448"),
    ("SunEC", "KeyAgreement", "OID.1.3.101.110", "X25519"),
    ("SunEC", "KeyAgreement", "OID.1.3.101.111", "X448"),
    // SunEC KeyFactory -- 9 rows
    ("SunEC", "KeyFactory", "1.3.101.110", "X25519"),
    ("SunEC", "KeyFactory", "1.3.101.111", "X448"),
    ("SunEC", "KeyFactory", "1.3.101.112", "Ed25519"),
    ("SunEC", "KeyFactory", "1.3.101.113", "Ed448"),
    ("SunEC", "KeyFactory", "OID.1.2.840.10045.2.1", "EC"),
    ("SunEC", "KeyFactory", "OID.1.3.101.110", "X25519"),
    ("SunEC", "KeyFactory", "OID.1.3.101.111", "X448"),
    ("SunEC", "KeyFactory", "OID.1.3.101.112", "Ed25519"),
    ("SunEC", "KeyFactory", "OID.1.3.101.113", "Ed448"),
    // SunEC KeyPairGenerator -- 9 rows
    ("SunEC", "KeyPairGenerator", "1.3.101.110", "X25519"),
    ("SunEC", "KeyPairGenerator", "1.3.101.111", "X448"),
    ("SunEC", "KeyPairGenerator", "1.3.101.112", "Ed25519"),
    ("SunEC", "KeyPairGenerator", "1.3.101.113", "Ed448"),
    ("SunEC", "KeyPairGenerator", "OID.1.2.840.10045.2.1", "EC"),
    ("SunEC", "KeyPairGenerator", "OID.1.3.101.110", "X25519"),
    ("SunEC", "KeyPairGenerator", "OID.1.3.101.111", "X448"),
    ("SunEC", "KeyPairGenerator", "OID.1.3.101.112", "Ed25519"),
    ("SunEC", "KeyPairGenerator", "OID.1.3.101.113", "Ed448"),
    // SunEC Signature -- 22 rows
    ("SunEC", "Signature", "1.2.840.10045.4.1", "SHA1withECDSA"),
    (
        "SunEC",
        "Signature",
        "1.2.840.10045.4.3.1",
        "SHA224withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "1.2.840.10045.4.3.2",
        "SHA256withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "1.2.840.10045.4.3.3",
        "SHA384withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "1.2.840.10045.4.3.4",
        "SHA512withECDSA",
    ),
    ("SunEC", "Signature", "1.3.101.112", "Ed25519"),
    ("SunEC", "Signature", "1.3.101.113", "Ed448"),
    (
        "SunEC",
        "Signature",
        "2.16.840.1.101.3.4.3.10",
        "SHA3-256withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "2.16.840.1.101.3.4.3.11",
        "SHA3-384withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "2.16.840.1.101.3.4.3.12",
        "SHA3-512withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "2.16.840.1.101.3.4.3.9",
        "SHA3-224withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "OID.1.2.840.10045.4.1",
        "SHA1withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "OID.1.2.840.10045.4.3.1",
        "SHA224withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "OID.1.2.840.10045.4.3.2",
        "SHA256withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "OID.1.2.840.10045.4.3.3",
        "SHA384withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "OID.1.2.840.10045.4.3.4",
        "SHA512withECDSA",
    ),
    ("SunEC", "Signature", "OID.1.3.101.112", "Ed25519"),
    ("SunEC", "Signature", "OID.1.3.101.113", "Ed448"),
    (
        "SunEC",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.10",
        "SHA3-256withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.11",
        "SHA3-384withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.12",
        "SHA3-512withECDSA",
    ),
    (
        "SunEC",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.9",
        "SHA3-224withECDSA",
    ),
    // SunJCE AlgorithmParameters -- 25 rows
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.1.7",
        "OAEP",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.12.1.1",
        "PBEWithSHA1AndRC4_128",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.12.1.2",
        "PBEWithSHA1AndRC4_40",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.12.1.3",
        "PBEWithSHA1AndDESede",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.12.1.5",
        "PBEWithSHA1AndRC2_128",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.12.1.6",
        "PBEWithSHA1AndRC2_40",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.3.1",
        "DiffieHellman",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.5.13",
        "PBES2",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.5.3",
        "PBEWithMD5AndDES",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "1.2.840.113549.1.9.16.3.18",
        "ChaCha20-Poly1305",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "2.16.840.1.101.3.4.1",
        "AES",
    ),
    ("SunJCE", "AlgorithmParameters", "DH", "DiffieHellman"),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.1.7",
        "OAEP",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.12.1.1",
        "PBEWithSHA1AndRC4_128",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.12.1.2",
        "PBEWithSHA1AndRC4_40",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.12.1.3",
        "PBEWithSHA1AndDESede",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.12.1.5",
        "PBEWithSHA1AndRC2_128",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.12.1.6",
        "PBEWithSHA1AndRC2_40",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.3.1",
        "DiffieHellman",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.5.13",
        "PBES2",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.5.3",
        "PBEWithMD5AndDES",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.1.2.840.113549.1.9.16.3.18",
        "ChaCha20-Poly1305",
    ),
    (
        "SunJCE",
        "AlgorithmParameters",
        "OID.2.16.840.1.101.3.4.1",
        "AES",
    ),
    ("SunJCE", "AlgorithmParameters", "PBE", "PBEWithMD5AndDES"),
    ("SunJCE", "AlgorithmParameters", "TripleDES", "DESede"),
    // SunJCE Cipher -- 43 rows
    (
        "SunJCE",
        "Cipher",
        "1.2.840.113549.1.9.16.3.18",
        "ChaCha20-Poly1305",
    ),
    ("SunJCE", "Cipher", "1.2.840.113549.3.4", "ARCFOUR"),
    ("SunJCE", "Cipher", "2.16.840.1.101.3.4.1", "AES"),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.1",
        "AES_128/ECB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.2",
        "AES_128/CBC/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.21",
        "AES_192/ECB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.22",
        "AES_192/CBC/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.23",
        "AES_192/OFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.24",
        "AES_192/CFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.25",
        "AES_192/KW/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.26",
        "AES_192/GCM/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.3",
        "AES_128/OFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.4",
        "AES_128/CFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.41",
        "AES_256/ECB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.42",
        "AES_256/CBC/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.43",
        "AES_256/OFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.44",
        "AES_256/CFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.45",
        "AES_256/KW/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.46",
        "AES_256/GCM/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.5",
        "AES_128/KW/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "2.16.840.1.101.3.4.1.6",
        "AES_128/GCM/NoPadding",
    ),
    ("SunJCE", "Cipher", "AESWrapPad", "AES/KWP/NoPadding"),
    (
        "SunJCE",
        "Cipher",
        "OID.1.2.840.113549.1.9.16.3.18",
        "ChaCha20-Poly1305",
    ),
    ("SunJCE", "Cipher", "OID.1.2.840.113549.3.4", "ARCFOUR"),
    ("SunJCE", "Cipher", "OID.2.16.840.1.101.3.4.1", "AES"),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.1",
        "AES_128/ECB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.2",
        "AES_128/CBC/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.21",
        "AES_192/ECB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.22",
        "AES_192/CBC/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.23",
        "AES_192/OFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.24",
        "AES_192/CFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.25",
        "AES_192/KW/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.26",
        "AES_192/GCM/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.3",
        "AES_128/OFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.4",
        "AES_128/CFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.41",
        "AES_256/ECB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.42",
        "AES_256/CBC/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.43",
        "AES_256/OFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.44",
        "AES_256/CFB/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.45",
        "AES_256/KW/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.46",
        "AES_256/GCM/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.5",
        "AES_128/KW/NoPadding",
    ),
    (
        "SunJCE",
        "Cipher",
        "OID.2.16.840.1.101.3.4.1.6",
        "AES_128/GCM/NoPadding",
    ),
    // SunJCE KEM -- 3 rows
    ("SunJCE", "KEM", "OID.2.16.840.1.101.3.4.4.1", "ML-KEM-512"),
    ("SunJCE", "KEM", "OID.2.16.840.1.101.3.4.4.2", "ML-KEM-768"),
    ("SunJCE", "KEM", "OID.2.16.840.1.101.3.4.4.3", "ML-KEM-1024"),
    // SunJCE KeyAgreement -- 2 rows
    (
        "SunJCE",
        "KeyAgreement",
        "1.2.840.113549.1.3.1",
        "DiffieHellman",
    ),
    (
        "SunJCE",
        "KeyAgreement",
        "OID.1.2.840.113549.1.3.1",
        "DiffieHellman",
    ),
    // SunJCE KeyFactory -- 8 rows
    (
        "SunJCE",
        "KeyFactory",
        "1.2.840.113549.1.3.1",
        "DiffieHellman",
    ),
    (
        "SunJCE",
        "KeyFactory",
        "2.16.840.1.101.3.4.4.1",
        "ML-KEM-512",
    ),
    (
        "SunJCE",
        "KeyFactory",
        "2.16.840.1.101.3.4.4.2",
        "ML-KEM-768",
    ),
    (
        "SunJCE",
        "KeyFactory",
        "2.16.840.1.101.3.4.4.3",
        "ML-KEM-1024",
    ),
    (
        "SunJCE",
        "KeyFactory",
        "OID.1.2.840.113549.1.3.1",
        "DiffieHellman",
    ),
    (
        "SunJCE",
        "KeyFactory",
        "OID.2.16.840.1.101.3.4.4.1",
        "ML-KEM-512",
    ),
    (
        "SunJCE",
        "KeyFactory",
        "OID.2.16.840.1.101.3.4.4.2",
        "ML-KEM-768",
    ),
    (
        "SunJCE",
        "KeyFactory",
        "OID.2.16.840.1.101.3.4.4.3",
        "ML-KEM-1024",
    ),
    // SunJCE KeyGenerator -- 15 rows
    (
        "SunJCE",
        "KeyGenerator",
        "1.2.840.113549.2.10",
        "HmacSHA384",
    ),
    (
        "SunJCE",
        "KeyGenerator",
        "1.2.840.113549.2.11",
        "HmacSHA512",
    ),
    ("SunJCE", "KeyGenerator", "1.2.840.113549.2.7", "HmacSHA1"),
    ("SunJCE", "KeyGenerator", "1.2.840.113549.2.8", "HmacSHA224"),
    ("SunJCE", "KeyGenerator", "1.2.840.113549.2.9", "HmacSHA256"),
    ("SunJCE", "KeyGenerator", "1.2.840.113549.3.4", "ARCFOUR"),
    ("SunJCE", "KeyGenerator", "2.16.840.1.101.3.4.1", "AES"),
    (
        "SunJCE",
        "KeyGenerator",
        "OID.1.2.840.113549.2.10",
        "HmacSHA384",
    ),
    (
        "SunJCE",
        "KeyGenerator",
        "OID.1.2.840.113549.2.11",
        "HmacSHA512",
    ),
    (
        "SunJCE",
        "KeyGenerator",
        "OID.1.2.840.113549.2.7",
        "HmacSHA1",
    ),
    (
        "SunJCE",
        "KeyGenerator",
        "OID.1.2.840.113549.2.8",
        "HmacSHA224",
    ),
    (
        "SunJCE",
        "KeyGenerator",
        "OID.1.2.840.113549.2.9",
        "HmacSHA256",
    ),
    (
        "SunJCE",
        "KeyGenerator",
        "OID.1.2.840.113549.3.4",
        "ARCFOUR",
    ),
    ("SunJCE", "KeyGenerator", "OID.2.16.840.1.101.3.4.1", "AES"),
    ("SunJCE", "KeyGenerator", "TripleDES", "DESede"),
    // SunJCE KeyPairGenerator -- 8 rows
    (
        "SunJCE",
        "KeyPairGenerator",
        "1.2.840.113549.1.3.1",
        "DiffieHellman",
    ),
    (
        "SunJCE",
        "KeyPairGenerator",
        "2.16.840.1.101.3.4.4.1",
        "ML-KEM-512",
    ),
    (
        "SunJCE",
        "KeyPairGenerator",
        "2.16.840.1.101.3.4.4.2",
        "ML-KEM-768",
    ),
    (
        "SunJCE",
        "KeyPairGenerator",
        "2.16.840.1.101.3.4.4.3",
        "ML-KEM-1024",
    ),
    (
        "SunJCE",
        "KeyPairGenerator",
        "OID.1.2.840.113549.1.3.1",
        "DiffieHellman",
    ),
    (
        "SunJCE",
        "KeyPairGenerator",
        "OID.2.16.840.1.101.3.4.4.1",
        "ML-KEM-512",
    ),
    (
        "SunJCE",
        "KeyPairGenerator",
        "OID.2.16.840.1.101.3.4.4.2",
        "ML-KEM-768",
    ),
    (
        "SunJCE",
        "KeyPairGenerator",
        "OID.2.16.840.1.101.3.4.4.3",
        "ML-KEM-1024",
    ),
    // SunJCE Mac -- 22 rows
    ("SunJCE", "Mac", "1.2.840.113549.2.10", "HmacSHA384"),
    ("SunJCE", "Mac", "1.2.840.113549.2.11", "HmacSHA512"),
    ("SunJCE", "Mac", "1.2.840.113549.2.12", "HmacSHA512/224"),
    ("SunJCE", "Mac", "1.2.840.113549.2.13", "HmacSHA512/256"),
    ("SunJCE", "Mac", "1.2.840.113549.2.7", "HmacSHA1"),
    ("SunJCE", "Mac", "1.2.840.113549.2.8", "HmacSHA224"),
    ("SunJCE", "Mac", "1.2.840.113549.2.9", "HmacSHA256"),
    ("SunJCE", "Mac", "2.16.840.1.101.3.4.2.13", "HmacSHA3-224"),
    ("SunJCE", "Mac", "2.16.840.1.101.3.4.2.14", "HmacSHA3-256"),
    ("SunJCE", "Mac", "2.16.840.1.101.3.4.2.15", "HmacSHA3-384"),
    ("SunJCE", "Mac", "2.16.840.1.101.3.4.2.16", "HmacSHA3-512"),
    ("SunJCE", "Mac", "OID.1.2.840.113549.2.10", "HmacSHA384"),
    ("SunJCE", "Mac", "OID.1.2.840.113549.2.11", "HmacSHA512"),
    ("SunJCE", "Mac", "OID.1.2.840.113549.2.12", "HmacSHA512/224"),
    ("SunJCE", "Mac", "OID.1.2.840.113549.2.13", "HmacSHA512/256"),
    ("SunJCE", "Mac", "OID.1.2.840.113549.2.7", "HmacSHA1"),
    ("SunJCE", "Mac", "OID.1.2.840.113549.2.8", "HmacSHA224"),
    ("SunJCE", "Mac", "OID.1.2.840.113549.2.9", "HmacSHA256"),
    (
        "SunJCE",
        "Mac",
        "OID.2.16.840.1.101.3.4.2.13",
        "HmacSHA3-224",
    ),
    (
        "SunJCE",
        "Mac",
        "OID.2.16.840.1.101.3.4.2.14",
        "HmacSHA3-256",
    ),
    (
        "SunJCE",
        "Mac",
        "OID.2.16.840.1.101.3.4.2.15",
        "HmacSHA3-384",
    ),
    (
        "SunJCE",
        "Mac",
        "OID.2.16.840.1.101.3.4.2.16",
        "HmacSHA3-512",
    ),
    // SunJCE SecretKeyFactory -- 14 rows
    (
        "SunJCE",
        "SecretKeyFactory",
        "1.2.840.113549.1.12.1.1",
        "PBEWithSHA1AndRC4_128",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "1.2.840.113549.1.12.1.2",
        "PBEWithSHA1AndRC4_40",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "1.2.840.113549.1.12.1.3",
        "PBEWithSHA1AndDESede",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "1.2.840.113549.1.12.1.5",
        "PBEWithSHA1AndRC2_128",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "1.2.840.113549.1.12.1.6",
        "PBEWithSHA1AndRC2_40",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "1.2.840.113549.1.5.12",
        "PBKDF2WithHmacSHA1",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "1.2.840.113549.1.5.3",
        "PBEWithMD5AndDES",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "OID.1.2.840.113549.1.12.1.1",
        "PBEWithSHA1AndRC4_128",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "OID.1.2.840.113549.1.12.1.2",
        "PBEWithSHA1AndRC4_40",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "OID.1.2.840.113549.1.12.1.3",
        "PBEWithSHA1AndDESede",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "OID.1.2.840.113549.1.12.1.5",
        "PBEWithSHA1AndRC2_128",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "OID.1.2.840.113549.1.12.1.6",
        "PBEWithSHA1AndRC2_40",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "OID.1.2.840.113549.1.5.12",
        "PBKDF2WithHmacSHA1",
    ),
    (
        "SunJCE",
        "SecretKeyFactory",
        "OID.1.2.840.113549.1.5.3",
        "PBEWithMD5AndDES",
    ),
    // SunRsaSign KeyFactory -- 6 rows
    ("SunRsaSign", "KeyFactory", "1.2.840.113549.1.1", "RSA"),
    ("SunRsaSign", "KeyFactory", "1.2.840.113549.1.1.1", "RSA"),
    (
        "SunRsaSign",
        "KeyFactory",
        "1.2.840.113549.1.1.10",
        "RSASSA-PSS",
    ),
    ("SunRsaSign", "KeyFactory", "OID.1.2.840.113549.1.1", "RSA"),
    (
        "SunRsaSign",
        "KeyFactory",
        "OID.1.2.840.113549.1.1.10",
        "RSASSA-PSS",
    ),
    ("SunRsaSign", "KeyFactory", "PSS", "RSASSA-PSS"),
    // SunRsaSign KeyPairGenerator -- 6 rows
    (
        "SunRsaSign",
        "KeyPairGenerator",
        "1.2.840.113549.1.1",
        "RSA",
    ),
    (
        "SunRsaSign",
        "KeyPairGenerator",
        "1.2.840.113549.1.1.1",
        "RSA",
    ),
    (
        "SunRsaSign",
        "KeyPairGenerator",
        "1.2.840.113549.1.1.10",
        "RSASSA-PSS",
    ),
    (
        "SunRsaSign",
        "KeyPairGenerator",
        "OID.1.2.840.113549.1.1",
        "RSA",
    ),
    (
        "SunRsaSign",
        "KeyPairGenerator",
        "OID.1.2.840.113549.1.1.10",
        "RSASSA-PSS",
    ),
    ("SunRsaSign", "KeyPairGenerator", "PSS", "RSASSA-PSS"),
    // SunRsaSign Signature -- 30 rows
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.10",
        "RSASSA-PSS",
    ),
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.11",
        "SHA256withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.12",
        "SHA384withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.13",
        "SHA512withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.14",
        "SHA224withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.15",
        "SHA512/224withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.16",
        "SHA512/256withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.2",
        "MD2withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.4",
        "MD5withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "1.2.840.113549.1.1.5",
        "SHA1withRSA",
    ),
    ("SunRsaSign", "Signature", "1.3.14.3.2.29", "SHA1withRSA"),
    (
        "SunRsaSign",
        "Signature",
        "2.16.840.1.101.3.4.3.13",
        "SHA3-224withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "2.16.840.1.101.3.4.3.14",
        "SHA3-256withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "2.16.840.1.101.3.4.3.15",
        "SHA3-384withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "2.16.840.1.101.3.4.3.16",
        "SHA3-512withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.10",
        "RSASSA-PSS",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.11",
        "SHA256withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.12",
        "SHA384withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.13",
        "SHA512withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.14",
        "SHA224withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.15",
        "SHA512/224withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.16",
        "SHA512/256withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.2",
        "MD2withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.4",
        "MD5withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.1.2.840.113549.1.1.5",
        "SHA1withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.13",
        "SHA3-224withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.14",
        "SHA3-256withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.15",
        "SHA3-384withRSA",
    ),
    (
        "SunRsaSign",
        "Signature",
        "OID.2.16.840.1.101.3.4.3.16",
        "SHA3-512withRSA",
    ),
    ("SunRsaSign", "Signature", "PSS", "RSASSA-PSS"),
];

/// Register [`MEASURED_JDK25_PROVIDER_ALIASES`].
///
/// Aliases stay OUT of `Security.getAlgorithms` (their property key is
/// `Alg.Alias.…`, see `algorithms_for_service`), so this widens the spellings
/// each provider answers to without moving any advertised count.
fn seed_measured_jdk25_provider_aliases() {
    for (provider, engine, alias, canonical) in MEASURED_JDK25_PROVIDER_ALIASES {
        put_alias(provider, engine, alias, canonical);
    }
}

/// W4-3 — the engine services that were only ever asserted by the retired
/// `Security.getAlgorithms` literal table in
/// `phases_early::register_phase53_security`.
///
/// That table is now gone and `algorithms_for_service` answers from this
/// registry instead. Every entry the table claimed must therefore exist here,
/// or `--synthetic-jdk` would answer a SHORTER list than before — the same
/// short-list defect one layer down. These are the entries that were claimed
/// but not registered:
///
///   * `Mac` — the registry had NO `Mac` service at all, so the type would
///     have gone 5 names -> 0.
///   * `KeyGenerator` `AES`/`DESede` — only `HmacSHA256` existed, and only via
///     `seed_sunjce_pbe_services`, which is gated on `ec_real`.
///   * `KeyPairGenerator` `RSA`/`DSA` — only `EC` existed (`seed_sunec_services`).
///
/// Provider ownership and SPI class names are MEASURED, not guessed:
/// enumerating `getServices()` per provider on jdk-25.0.3.9-hotspot. Every
/// name below is one HotSpot really answers for that type.
///
/// Three things the retired table claimed that are deliberately NOT seeded,
/// because the same measurement says HotSpot does not answer them:
///   * `Cipher` `AES/CBC/PKCS5Padding`, `AES/CBC/NoPadding`,
///     `AES/ECB/PKCS5Padding` — HotSpot's `Cipher` set has no `AES/CBC/*` or
///     `AES/ECB/*` entry; those transformations are serviced by the generic
///     `Cipher.AES` service, which is already registered.
///   * `SecureRandom` `NativePRNGNonBlocking`/`NativePRNGBlocking` — SUN
///     registers those only on unix-like images; the platform JDK answers
///     `[DRBG, SHA1PRNG, WINDOWS-PRNG]`. `find_service_provider("SecureRandom",
///     ..)` is what `securerandom.rs` uses to decide whether `getInstance`
///     succeeds, so seeding a name we do not implement would fabricate a
///     working PRNG. The set stays non-empty either way, which is all
///     Tomcat's `SessionIdGeneratorBase.<clinit>` needs.
///
/// A third exclusion used to stand here: `MessageDigest` `SHAKE128-256` /
/// `SHAKE256-512`, declined because `message_digest::algorithm_supported` did
/// not implement them and — in the words of the comment this replaces —
/// "advertising a digest that `getInstance` then refuses is a worse lie than
/// a 13-name list". That reasoning is exactly right and is the rule this file
/// runs on. Its PREMISE expired on 2026-08-12, when both were implemented
/// against the HotSpot and NIST vectors; they are now seeded in
/// `seed_direct_native_engine_services` beside the other SUN digests, and
/// their bare `SHAKE128`/`SHAKE256` spellings are ALIASES there, not
/// services, so the advertised count lands on HotSpot's 15 rather than 17. A
/// comment outlives its defect. W7-63-jca-advertise-vs-serve.md.
fn seed_retired_getalgorithms_literals() {
    const JCE: &str = "SunJCE";
    // Every class name below is MEASURED — `getServices()` enumerated per
    // provider on jdk-25.0.3.9-hotspot and printed as
    // `type|algorithm|className`, not read off a javap of the provider's
    // `<clinit>` and not recalled. `Mac.getInstance` is natively intercepted
    // (`phases_late::ssl_security`), so for this type the class name is
    // documentation rather than a load target; it is still worth being right,
    // because the same row answers `Provider.Service.getClassName()`.
    for (algorithm, class_name) in [
        ("HmacMD5", "com.sun.crypto.provider.HmacMD5"),
        ("HmacSHA1", "com.sun.crypto.provider.HmacSHA1"),
        // W7-39: `HmacSHA224` is implemented by `ssl_security::mac_compute_hmac`
        // as of 2026-08-12 and must be advertised in the same commit. The
        // reverse drift — implemented but unadvertised — is the quiet half of
        // this defect species, because nothing asks for a name nobody publishes.
        ("HmacSHA224", "com.sun.crypto.provider.HmacCore$HmacSHA224"),
        ("HmacSHA256", "com.sun.crypto.provider.HmacCore$HmacSHA256"),
        ("HmacSHA384", "com.sun.crypto.provider.HmacCore$HmacSHA384"),
        ("HmacSHA512", "com.sun.crypto.provider.HmacCore$HmacSHA512"),
        // The two FIPS 180-4 §5.3.6 truncations and the SHA-3 family. Added
        // alongside the matching `mac_compute_hmac` arms, never ahead of them:
        // this list and `phases_late::ssl_security::mac_algorithm_supported`
        // must answer the same set, which
        // `mac_supported_set_matches_the_advertised_sunjce_services` asserts.
        (
            "HmacSHA512/224",
            "com.sun.crypto.provider.HmacCore$HmacSHA512_224",
        ),
        (
            "HmacSHA512/256",
            "com.sun.crypto.provider.HmacCore$HmacSHA512_256",
        ),
        (
            "HmacSHA3-224",
            "com.sun.crypto.provider.HmacCore$HmacSHA3_224",
        ),
        (
            "HmacSHA3-256",
            "com.sun.crypto.provider.HmacCore$HmacSHA3_256",
        ),
        (
            "HmacSHA3-384",
            "com.sun.crypto.provider.HmacCore$HmacSHA3_384",
        ),
        (
            "HmacSHA3-512",
            "com.sun.crypto.provider.HmacCore$HmacSHA3_512",
        ),
    ] {
        put_service(JCE, "Mac", algorithm, class_name);
    }
    // `KeyGenerator` is NOT natively intercepted in `--real-jdk` mode: the real
    // `KeyGenerator.getInstance` reaches `sun.security.jca.GetInstance`, which
    // this module answers from the registry below and then INSTANTIATES the
    // named class. So every row here has to be a class the real image really
    // ships with a public no-arg ctor — the SunJCE key generators all are,
    // being reflectively constructed by the provider itself — and the bytes a
    // caller gets are the JDK's own, not a reimplementation.
    //
    // The list was `[AES, DESede, HmacSHA256]` until 2026-08-12, which made
    // `KeyGenerator.getInstance("Blowfish")` — and `DES`, `ChaCha20`, and every
    // Hmac name but one — answer `NoSuchAlgorithmException: <name> KeyGenerator
    // not available` (measured) where HotSpot hands back a key. That was the
    // advertised-vs-implemented gap in its quiet direction twice over:
    // `phases_early::keygen_default_bits` (the `--synthetic-jdk` path) already
    // implemented all of these, and the real image implements all of them too;
    // only the registry the two modes share disagreed.
    for (algorithm, class_name) in [
        ("AES", "com.sun.crypto.provider.AESKeyGenerator"),
        (
            "ARCFOUR",
            "com.sun.crypto.provider.KeyGeneratorCore$ARCFOURKeyGenerator",
        ),
        ("Blowfish", "com.sun.crypto.provider.BlowfishKeyGenerator"),
        (
            "ChaCha20",
            "com.sun.crypto.provider.KeyGeneratorCore$ChaCha20KeyGenerator",
        ),
        ("DES", "com.sun.crypto.provider.DESKeyGenerator"),
        ("DESede", "com.sun.crypto.provider.DESedeKeyGenerator"),
        ("HmacMD5", "com.sun.crypto.provider.HmacMD5KeyGenerator"),
        ("HmacSHA1", "com.sun.crypto.provider.HmacSHA1KeyGenerator"),
        (
            "HmacSHA224",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA224",
        ),
        (
            "HmacSHA256",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA256",
        ),
        (
            "HmacSHA384",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA384",
        ),
        (
            "HmacSHA512",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA512",
        ),
        (
            "RC2",
            "com.sun.crypto.provider.KeyGeneratorCore$RC2KeyGenerator",
        ),
        // The six that were withheld until 2026-09-02 because
        // `keygen_default_bits` had no arm for them. It does now — every one is
        // the digest's own output length, measured against HotSpot rather than
        // derived — so the precondition the note below states is discharged and
        // the rows are truthful.
        (
            "HmacSHA512/224",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA512_224",
        ),
        (
            "HmacSHA512/256",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA512_256",
        ),
        (
            "HmacSHA3-224",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA3_224",
        ),
        (
            "HmacSHA3-256",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA3_256",
        ),
        (
            "HmacSHA3-384",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA3_384",
        ),
        (
            "HmacSHA3-512",
            "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA3_512",
        ),
    ] {
        put_service(JCE, "KeyGenerator", algorithm, class_name);
    }
    // SunJCE's own `KeyGenerator` alias, and the reason `keygen_default_bits`
    // spells both: `RC4` is not a service, it is
    // `Alg.Alias.KeyGenerator.RC4 = ARCFOUR`. Aliases stay out of
    // `Security.getAlgorithms` (their property key is `Alg.Alias.…`), which is
    // why HotSpot's own list names ARCFOUR and not RC4.
    put_alias(JCE, "KeyGenerator", "RC4", "ARCFOUR");
    // The five `SunTls*` generators: TLS-internal KDFs driven by
    // `sun.security.ssl`, taking `TlsKeyMaterialParameterSpec`-family specs
    // that this engine's two-field `init` surface cannot carry.
    //
    // They were the last unseeded `KeyGenerator` names on HotSpot's list, and
    // the note here used to say why they had to stay that way: serving them
    // means handing back a REAL `javax.crypto.KeyGenerator` over the platform's
    // SPI, which every native on this class would then have to recognise. That
    // is what `keygen_real_spi` now does, so the rows are real rows naming real
    // classes and the engine falls to them on its own refusal.
    //
    // `HmacSHA3-{224,256,384,512}` and `HmacSHA512/{224,256}` were on that list
    // for the same kind of reason — `keygen_default_bits` had no arm — and were
    // seeded above once it did. This is the same move one engine over.
    for (algorithm, class_name) in [
        ("SunTlsPrf", "com.sun.crypto.provider.TlsPrfGenerator$V10"),
        ("SunTls12Prf", "com.sun.crypto.provider.TlsPrfGenerator$V12"),
        (
            "SunTlsMasterSecret",
            "com.sun.crypto.provider.TlsMasterSecretGenerator",
        ),
        (
            "SunTlsKeyMaterial",
            "com.sun.crypto.provider.TlsKeyMaterialGenerator",
        ),
        (
            "SunTlsRsaPremasterSecret",
            "com.sun.crypto.provider.TlsRsaPremasterSecretGenerator",
        ),
    ] {
        put_service(JCE, "KeyGenerator", algorithm, class_name);
    }
    put_service(
        "SunRsaSign",
        "KeyPairGenerator",
        "RSA",
        "sun.security.rsa.RSAKeyPairGenerator$Legacy",
    );
    put_service(
        "SUN",
        "KeyPairGenerator",
        "DSA",
        "sun.security.provider.DSAKeyPairGenerator$Current",
    );
    seed_builtin_keypairgenerator_services();
}

/// Service entries for every `KeyPairGenerator` algorithm this VM SERVES from
/// its own natives.
///
/// WHY THIS EXISTS. `Security.getProviders("KeyPairGenerator.Ed25519")`
/// answered `<none>` for an algorithm `getInstance("Ed25519")` happily served,
/// because the two live in disjoint worlds: `security_get_providers_filtered`
/// consults only the service registry, and the algorithms CratonVM serves from
/// `key_factory::kpg_can_generate` are in no registry at all. A caller that
/// picks its provider by filter — the JDK's own `Provider.getService` shape —
/// therefore concluded the algorithm did not exist, and `getInstance` then
/// served it anyway.
///
/// Seeding them closes the filter AND collapses the split `kpg_serviceable`
/// has to straddle: the second half of that disjunction
/// (`any_provider_offers`) now covers the first for every built-in name, so
/// the two worlds agree even though the code still asks both.
///
/// Provider assignment is HotSpot's, read off JDK 25 with
/// `probes/JcaGetInstanceProbe.java` and pinned to `kpg_provider_name` by
/// `every_kpg_algorithm_this_vm_serves_is_advertised`. Class names are the
/// real SPI classes each provider registers; nothing reads them on the
/// intercepted path (`kpg_get_instance` never reaches `build_jca_impl`), which
/// is exactly why the RATCHET and not the string is what keeps a row honest.
fn seed_builtin_keypairgenerator_services() {
    put_service(
        "SunRsaSign",
        "KeyPairGenerator",
        "RSASSA-PSS",
        "sun.security.rsa.RSAKeyPairGenerator$PSS",
    );
    for (algorithm, class_name) in [
        (
            "Ed25519",
            "sun.security.ec.ed.EdDSAKeyPairGenerator$Ed25519",
        ),
        ("Ed448", "sun.security.ec.ed.EdDSAKeyPairGenerator$Ed448"),
        ("EdDSA", "sun.security.ec.ed.EdDSAKeyPairGenerator"),
        ("X25519", "sun.security.ec.XDHKeyPairGenerator$X25519"),
        ("X448", "sun.security.ec.XDHKeyPairGenerator$X448"),
        ("XDH", "sun.security.ec.XDHKeyPairGenerator"),
    ] {
        put_service("SunEC", "KeyPairGenerator", algorithm, class_name);
    }
    for (algorithm, class_name) in [
        ("ML-DSA", "sun.security.provider.ML_DSA_Impls$KPG"),
        ("ML-DSA-44", "sun.security.provider.ML_DSA_Impls$KPG2"),
        ("ML-DSA-65", "sun.security.provider.ML_DSA_Impls$KPG3"),
        ("ML-DSA-87", "sun.security.provider.ML_DSA_Impls$KPG5"),
    ] {
        put_service("SUN", "KeyPairGenerator", algorithm, class_name);
    }
    for (algorithm, class_name) in [
        ("ML-KEM", "com.sun.crypto.provider.ML_KEM_Impls$KPG"),
        ("ML-KEM-512", "com.sun.crypto.provider.ML_KEM_Impls$KPG2"),
        ("ML-KEM-768", "com.sun.crypto.provider.ML_KEM_Impls$KPG3"),
        ("ML-KEM-1024", "com.sun.crypto.provider.ML_KEM_Impls$KPG5"),
        (
            "DiffieHellman",
            "com.sun.crypto.provider.DHKeyPairGenerator",
        ),
    ] {
        put_service("SunJCE", "KeyPairGenerator", algorithm, class_name);
    }
    // `DH` is an alias on HotSpot too, so it resolves through `getInstance`
    // without lengthening `Security.getAlgorithms("KeyPairGenerator")`.
    put_alias("SunJCE", "KeyPairGenerator", "DH", "DiffieHellman");
    // `ECDSA` is deliberately NOT here. SunEC registers `KeyPairGenerator.EC`
    // and no `ECDSA` alias for it, so HotSpot answers
    // `NoSuchAlgorithmException` — advertising it would make the filter claim
    // a name HotSpot refuses, which is this table's own failure mode in the
    // other direction.
}

/// Real-JCA bring-up: seed the `SunEC` provider's EC service entries into the
/// global service map.  SunEC registers these via `putService(Provider$Service)`
/// (not the legacy `put`/`parseLegacyPut` we intercept), so constructing the
/// real `sun.security.ec.SunEC` provider would populate the *real* Hashtable our
/// bridge never reads.  Instead we mirror SunEC's `putEntries()` table here so
/// the no-provider `getInstance("EC")` search (`getinstance_instance_search`)
/// resolves SunEC and `build_jca_instance` instantiates the real, pure-Java
/// JDK 25 SPIs (`ECKeyPairGenerator`/`ECKeyFactory`/`ECParameters`/
/// `ECDSASignature$*` — none use native methods, verified via `javap`).
///
/// Class names captured from `javap -c sun.security.ec.SunEC` on JDK 25.0.1.
fn seed_sunec_services() {
    const P: &str = "SunEC";
    put_service(P, "KeyFactory", "EC", "sun.security.ec.ECKeyFactory");
    put_service(
        P,
        "AlgorithmParameters",
        "EC",
        "sun.security.util.ECParameters",
    );
    put_service(
        P,
        "KeyPairGenerator",
        "EC",
        "sun.security.ec.ECKeyPairGenerator",
    );
    // The ECDSA `Signature` family, DERIVED rather than listed.
    //
    // This was sixteen hand-written `(name, class)` pairs and HotSpot has
    // twenty: the four `SHA3-*withECDSAinP1363Format` rows were missing, which
    // is a transcription gap and exactly what a hand-written list produces.
    // Both halves now come from `signature::ECDSA_FAMILY_SIGNATURE_NAMES` and
    // `ecdsa_family_service_class`, which is the same pair the ENGINE resolves
    // its SPI through — so the advertised set and the served set are one list
    // by construction, and `every_ecdsa_family_signature_name_maps_to_an_spi_class`
    // pins it.
    for algorithm in crate::jca::signature::ECDSA_FAMILY_SIGNATURE_NAMES {
        let cls = crate::jca::signature::ecdsa_family_service_class(algorithm).expect(
            "ECDSA_FAMILY_SIGNATURE_NAMES is exactly ecdsa_family_spi_class's domain",
        );
        put_service(P, "Signature", algorithm, &cls);
    }
    // `KeyAgreement` — four services this VM has been SERVING all along and
    // never advertised. Measured 2026-09-02: `KeyAgreement.getInstance` returns
    // a working agreement for every one of them, and none appeared in
    // `Security.getProvider("SunEC").getServices()`. That is the
    // serves-but-never-advertises half of this file's own title, and the same
    // shape as `SunJCE`'s unlisted `DiffieHellman`.
    for (algo, cls) in [
        ("ECDH", "sun.security.ec.ECDHKeyAgreement"),
        ("XDH", "sun.security.ec.XDHKeyAgreement"),
        ("X25519", "sun.security.ec.XDHKeyAgreement.X25519"),
        ("X448", "sun.security.ec.XDHKeyAgreement.X448"),
    ] {
        put_service(P, "KeyAgreement", algo, cls);
    }
    // EC name aliases consumed by getInstance("EC")/key-spec resolution.
    for ty in ["KeyFactory", "KeyPairGenerator", "AlgorithmParameters"] {
        put_alias(P, ty, "1.2.840.10045.2.1", "EC"); // X9.62 id-ecPublicKey OID
        put_alias(P, ty, "EllipticCurve", "EC");
    }
}

/// Mirror the real `SUN` provider's `AlgorithmParameters.DSA` service entry
/// (`sun.security.provider.SunEntries`) into our synthetic provider map.
/// `KeyFactory`/`Signature` for DSA are handled by dedicated native
/// short-circuits (`key_factory::kf_generate_public`/`signature::sig_verify`,
/// gated on `route_dsa_to_real()`), but `sun.security.x509.AlgorithmId
/// .decodeParams()` independently calls `AlgorithmParameters
/// .getInstance("DSA")` (real bytecode, via this `GetInstance` bridge) to
/// decode a DSA key's embedded `p`/`q`/`g` params. Without this entry that
/// throws `NoSuchAlgorithmException`, silently caught by `decodeParams()`,
/// leaving `DSAPublicKey.getParams()` null and
/// `sun.security.provider.DSA.engineInitVerify` throwing
/// `InvalidKeyException: DSA public key lacks parameters` — found
/// root-causing `SecurityInfoTests.getWhenJarIsSigned`'s DSA-signed bcprov
/// jar. `sun.security.provider.DSAParameters` is pure Java/ASN.1 (no native
/// methods, has the implicit public no-arg ctor JCA requires), verified via
/// source inspection of JDK 25.0.1's `src.zip`.
fn seed_sun_dsa_services() {
    const P: &str = "SUN";
    put_service(
        P,
        "AlgorithmParameters",
        "DSA",
        "sun.security.provider.DSAParameters",
    );
    put_alias(P, "AlgorithmParameters", "1.2.840.10040.4.1", "DSA"); // id-dsa OID
}

/// Mirror the SunJCE PKCS#12 PBES2 `AlgorithmParameters` service table so
/// `AlgorithmParameters.getInstance("PBEWithHmacSHA256AndAES_256")` (the
/// default PKCS12 keystore entry-protection algorithm since JDK 8u+) resolves
/// instead of dead-ending in "no AlgorithmParameters ... implementation in
/// any provider". Without this, loading ANY password-protected PKCS12
/// keystore entry (`KeyStore.setKeyEntry`/`KeyStore.load` on the default
/// keystore type) throws that error, which happens on a NON-main JUnit
/// worker thread for test fixtures like
/// `RestClientBuilderIntegTests.getSslContext()` — surfacing not as a clean
/// test failure but as the whole suite hanging (the uncaught native error
/// escapes the worker thread without the usual Java exception unwinding the
/// test framework's synchronization expects). Every subclass here has the
/// no-arg public ctor JCA requires and is pure ASN.1/DER parsing (no native
/// methods), verified via `javap` on JDK 25 (`com.sun.crypto.provider.
/// PBES2Parameters$HmacSHA*AndAES_*`).
fn seed_sunjce_pbe_services() {
    const P: &str = "SunJCE";
    // `AlgorithmParameters.getInstance("OAEP")` -- needed by RSA-OAEP-256 JWE
    // (e.g. WildFly Elytron's `ElytronRsaKeyEncryption256JWEAlgorithmProvider`
    // builds an `OAEPParameterSpec` and inits an `AlgorithmParameters` from it
    // directly). Verified via `javap -c com.sun.crypto.provider.SunJCE` on JDK
    // 25.0.1 (`ps("AlgorithmParameters", "OAEP", "com.sun.crypto.provider.
    // OAEPParameters")`); `OAEPParameters` has the required public no-arg ctor
    // and is pure ASN.1 (no native methods), same as the PBES2 classes below.
    put_service(
        P,
        "AlgorithmParameters",
        "OAEP",
        "com.sun.crypto.provider.OAEPParameters",
    );
    // `KeyGenerator.getInstance("HmacSHA256")` -- keycloak's
    // `ElytronHmacTest::testHmacSignaturesUsingKeyGen` generates an HMAC key
    // via `KeyGenerator` (as opposed to raw `SecretKeySpec`/`Mac`, which
    // already work without this entry). Verified via `javap -c
    // com.sun.crypto.provider.SunJCE` on JDK 25.0.1 (`ps("KeyGenerator",
    // "HmacSHA256", "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$
    // SHA256")`); the nested class has the required public no-arg ctor and is
    // pure Java (no native methods).
    put_service(
        P,
        "KeyGenerator",
        "HmacSHA256",
        "com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA256",
    );
    // `AlgorithmParameters.getInstance("PBES2")` — the GENERIC PBES2 entry
    // (verified via `javap -c com.sun.crypto.provider.SunJCE` on JDK 25.0.1:
    // `ps("AlgorithmParameters", "PBES2", "com.sun.crypto.provider.
    // PBES2Parameters$General")`), distinct from the per-hash/keysize
    // `PBEWithHmac*AndAES_*` entries below. `sun.security.x509.AlgorithmId`
    // decodes an encrypted PKCS#8 key's outer `AlgorithmIdentifier` params by
    // calling `AlgorithmParameters.getInstance(<name resolved from the OID>)`
    // — and the PBES2 OID (1.2.840.113549.1.5.13) resolves to the literal
    // name `"PBES2"`, NOT to the specific PRF+cipher combination (that's only
    // known once `$General.engineInit` parses the params' own inner ASN.1).
    // Without this entry, `AlgorithmId.decodeParams()` silently swallows the
    // NoSuchAlgorithmException and leaves `algParams` null; Spring Boot's
    // `PemPrivateKeyParser.Pkcs8PrivateKeyDecryptor.getEncryptionAlgorithm`
    // then falls back to the literal algorithm name `"PBES2"` instead of
    // `algParameters.toString()` (which would have produced the correct
    // `"PBEWithHmacSHA256AndAES_256"`-shaped name), so
    // `SecretKeyFactory.getInstance("PBES2")` throws `"PBES2 SecretKeyFactory
    // not available"` for EVERY encrypted PEM private key regardless of its
    // actual PRF/cipher. `PBES2Parameters$General` has the same public no-arg
    // ctor + pure-ASN.1 (no native methods) shape as the specific nested
    // classes below.
    put_service(
        P,
        "AlgorithmParameters",
        "PBES2",
        "com.sun.crypto.provider.PBES2Parameters$General",
    );
    // SunJCE's SYMMETRIC `AlgorithmParameters` services. Enumerated from
    // `Security.getProvider("SunJCE").getServices()` on JDK 25, not guessed —
    // the block-cipher rows were missing here entirely, so
    // `AlgorithmParameters.getInstance("AES", "SunJCE")` answered
    // `no such algorithm: AES for provider SunJCE` while HotSpot serves it.
    // BouncyCastle's `EnvelopedDataHelper.createAlgorithmParameters` asks by
    // exactly that (name, provider) pair to decode a CMS content-encryption
    // AlgorithmIdentifier, so every `SunProviderTest`/`NullProviderTest` KeyTrans
    // case in bc-java's `cms` suite lost the IV and failed to decrypt.
    //
    // Note `GCM` lives in `sun.security.util`, not `com.sun.crypto.provider` —
    // the one row whose package differs from its siblings.
    for (algo, cls) in [
        ("AES", "com.sun.crypto.provider.AESParameters"),
        ("GCM", "sun.security.util.GCMParameters"),
        ("DESede", "com.sun.crypto.provider.DESedeParameters"),
        ("DES", "com.sun.crypto.provider.DESParameters"),
        ("Blowfish", "com.sun.crypto.provider.BlowfishParameters"),
        ("RC2", "com.sun.crypto.provider.RC2Parameters"),
        (
            "ChaCha20-Poly1305",
            "com.sun.crypto.provider.ChaCha20Poly1305Parameters",
        ),
        ("DiffieHellman", "com.sun.crypto.provider.DHParameters"),
    ] {
        put_service(P, "AlgorithmParameters", algo, cls);
    }
    // The PKCS#12 / PKCS#5 v1.5 PBE rows, which all share one SPI class.
    for algo in [
        "PBEWithMD5AndDES",
        "PBEWithMD5AndTripleDES",
        "PBEWithSHA1AndDESede",
        "PBEWithSHA1AndRC2_40",
        "PBEWithSHA1AndRC2_128",
        "PBEWithSHA1AndRC4_40",
        "PBEWithSHA1AndRC4_128",
    ] {
        put_service(
            P,
            "AlgorithmParameters",
            algo,
            "com.sun.crypto.provider.PBEParameters",
        );
    }
    // TWO spellings, and conflating them cost four services.
    //
    // The ALGORITHM name carries a slash — `PBEWithHmacSHA512/224AndAES_128` —
    // because that is the digest's own name (`SHA-512/224`, FIPS 180-4) with
    // the JCA's `SHA-` elision. The nested CLASS name cannot: `/` is not a Java
    // identifier character, so the JDK writes `PBES2Parameters$HmacSHA512_224-
    // AndAES_128`. This loop derived both from one array spelled the class's
    // way, so the four `SHA512/{224,256}` rows were registered under names
    // HotSpot does not have and the names HotSpot DOES have were absent.
    //
    // The cost was symmetric and both halves were measured on 2026-09-02:
    // `Security.getProvider("SunJCE").getServices()` advertised four rows
    // HotSpot has never advertised, and
    // `AlgorithmParameters.getInstance("PBEWithHmacSHA512/224AndAES_128")`
    // raised `NoSuchAlgorithmException` where HotSpot serves it. That engine's
    // `getInstance` is NOT intercepted by this crate — it is ordinary JDK
    // bytecode walking `Provider.getService` — so the row is the whole
    // mechanism, and a misspelt row is the whole defect.
    //
    // `jca-provider-population-gap-20260830.md` §5 concluded "0 of 84 are
    // clerical" from reading two other engines' dispatch. These four were.
    const HASHES: &[(&str, &str)] = &[
        // (algorithm spelling, class-name spelling)
        ("SHA1", "SHA1"),
        ("SHA224", "SHA224"),
        ("SHA256", "SHA256"),
        ("SHA384", "SHA384"),
        ("SHA512", "SHA512"),
        ("SHA512/224", "SHA512_224"),
        ("SHA512/256", "SHA512_256"),
    ];
    const KEYSIZES: &[&str] = &["128", "256"];
    for (hash, hash_cls) in HASHES {
        for keysize in KEYSIZES {
            let algo = format!("PBEWithHmac{hash}AndAES_{keysize}");
            // The nested class name drops the "PBEWith" prefix, e.g.
            // `PBES2Parameters$HmacSHA256AndAES_256` (verified via `javap`),
            // NOT `PBES2Parameters$PBEWithHmacSHA256AndAES_256`.
            let cls =
                format!("com.sun.crypto.provider.PBES2Parameters$Hmac{hash_cls}AndAES_{keysize}");
            put_service(P, "AlgorithmParameters", &algo, &cls);
        }
    }
}

/// The seventeen SunJCE `Cipher` services this engine does not compute, seeded
/// with the JDK's REAL implementation classes so the chain walk can serve them.
///
/// `Cipher.getInstance`'s anonymous overload already falls to
/// `try_delegate_cipher_to_chain` when `classify_transformation` refuses a
/// name — the route added for X.509/PKCS/CMS callers who name their content
/// cipher by OID. It walks every installed provider and asks `build_jca_impl`
/// to instantiate the class the service row names, so for a name this VM has
/// no engine for, the row IS the implementation.
///
/// # Why the class name is the whole difference
///
/// `jca-provider-population-gap-20260830.md` §5.2 ran exactly this experiment
/// on six of these rows, measured `NoSuchAlgorithmException` unchanged, and
/// concluded that "a service row is not sufficient even for the one engine
/// that walks the chain" — from which §5.3 drew "0 of 84 are clerical".
///
/// The conclusion holds only for a row carrying `com.sun.crypto.provider
/// .Native`, which is what the rest of this file seeds and is not a class at
/// all: it is the marker meaning "a Rust engine answers this". The chain walk
/// finds such a row, asks for a class that does not exist, gets a
/// class-not-found, and moves to the next provider — ending at the caller's
/// original refusal, which is precisely what §5.2 saw. Rows naming
/// `KeyWrapCipher$AES128_KW_PKCS5Padding` and its sixteen siblings instantiate,
/// and all seventeen resolve (measured 2026-09-02).
///
/// Three families, none of them an algorithm this crate implements:
///
/// * **PBES2** (`PBEWithHmacSHA{384,512,512/224,512/256}AndAES_{128,256}`) —
///   PBKDF2 with the named PRF, then AES/CBC. The `SHA1`/`SHA224`/`SHA256`
///   members are computed natively and stay on the marker; these eight are the
///   PRFs `cipher.rs`'s own `pbes2_params` table has no arm for.
/// * **PKCS#12 / PKCS#5 v1.5 PBE** (`PBEWithMD5AndDES` and six siblings) — the
///   PBKDF1 and PKCS#12 B.2 derivations feeding DES, DESede, RC2 and RC4.
/// * **The rest**: `DESedeWrap` (RFC 3217 CMS key wrap) and `RC2`.
fn seed_sunjce_delegated_cipher_services() {
    const P: &str = "SunJCE";
    // PBES2, `(algorithm PRF spelling, class PRF spelling)` — the same
    // slash-versus-underscore split every PBES2 table in this file carries.
    for (algo_hash, class_hash) in [
        ("SHA384", "SHA384"),
        ("SHA512", "SHA512"),
        ("SHA512/224", "SHA512_224"),
        ("SHA512/256", "SHA512_256"),
    ] {
        for keysize in ["128", "256"] {
            put_service(
                P,
                "Cipher",
                &format!("PBEWithHmac{algo_hash}AndAES_{keysize}"),
                &format!(
                    "com.sun.crypto.provider.PBES2Core$Hmac{class_hash}AndAES_{keysize}"
                ),
            );
        }
    }
    for (algo, class) in [
        (
            "PBEWithMD5AndDES",
            "com.sun.crypto.provider.PBEWithMD5AndDESCipher",
        ),
        (
            "PBEWithMD5AndTripleDES",
            "com.sun.crypto.provider.PBEWithMD5AndTripleDESCipher",
        ),
        (
            "PBEWithSHA1AndDESede",
            "com.sun.crypto.provider.PKCS12PBECipherCore$PBEWithSHA1AndDESede",
        ),
        (
            "PBEWithSHA1AndRC2_40",
            "com.sun.crypto.provider.PKCS12PBECipherCore$PBEWithSHA1AndRC2_40",
        ),
        (
            "PBEWithSHA1AndRC2_128",
            "com.sun.crypto.provider.PKCS12PBECipherCore$PBEWithSHA1AndRC2_128",
        ),
        (
            "PBEWithSHA1AndRC4_40",
            "com.sun.crypto.provider.PKCS12PBECipherCore$PBEWithSHA1AndRC4_40",
        ),
        (
            "PBEWithSHA1AndRC4_128",
            "com.sun.crypto.provider.PKCS12PBECipherCore$PBEWithSHA1AndRC4_128",
        ),
        ("DESedeWrap", "com.sun.crypto.provider.DESedeWrapCipher"),
        ("RC2", "com.sun.crypto.provider.RC2Cipher"),
    ] {
        put_service(P, "Cipher", algo, class);
    }
}

/// SunJCE's sixteen password-based and SSL `Mac` services.
///
/// None of them is an HMAC this crate computes: `HmacPBESHA*` is the PKCS#12
/// v1.0 §B.2 key derivation feeding an HMAC, `PBEWithHmacSHA*` is PBMAC1
/// (PBKDF2 feeding an HMAC), and `SslMac{MD5,SHA1}` is the SSL 3.0 MAC, which
/// is not HMAC at all (concatenation with pad bytes, not the XOR construction).
/// Three separate derivations, none of them a name-table entry away.
///
/// They do not need to be. `phases_late::ssl_security`'s `Mac.getInstance`
/// already falls to `find_service_provider("Mac", algo)` +
/// `build_real_mac` for a name it cannot compute — the route added for
/// BouncyCastle's MAC families — and that route now admits a JDK provider too
/// (`jdk_service_class`), so a row here IS the implementation, driven from the
/// platform's own class. Ordered after this engine's verdict, so no name it
/// computes changes hands.
/// The four SunJCE services behind engines `JcaResolveAll` reports as SKIP:
/// three `KDF` (HKDF, JEP 478, final in JDK 25) and `KEM.DHKEM` (RFC 9180).
///
/// A skip is not a pass, and these were the proof: nine services sit behind
/// `KDF`/`KEM`/`Configuration` and the gap census could say nothing about any
/// of them because their APIs are not `getInstance(String)`-shaped. Asked
/// directly (`apps/probes/JcaModernEngines`), five of the nine refused.
///
/// `javax.crypto.KDF.getInstance` is not intercepted by this crate, so the row
/// is the whole implementation and the platform's own
/// `HKDFKeyDerivation$HKDFSHA*` serves it — verified against HotSpot on the
/// RFC 5869 extract-then-expand vector, byte for byte. `KEM` IS intercepted,
/// and `DHKEM` needed an arm in `kem::kem_algo_idx` beside the row.
fn seed_sunjce_modern_engine_services() {
    const P: &str = "SunJCE";
    for hash in ["SHA256", "SHA384", "SHA512"] {
        put_service(
            P,
            "KDF",
            &format!("HKDF-{hash}"),
            &format!("com.sun.crypto.provider.HKDFKeyDerivation$HKDF{hash}"),
        );
    }
    put_service(P, "KEM", "DHKEM", "com.sun.crypto.provider.DHKEM");
    // The four ML-KEM services, which this VM SERVES — `jca::kem` drives the
    // real `ML_KEM_Impls$K*` SPI and `apps/probes/JcaModernEngines` measures a
    // full encapsulate/decapsulate agreeing with HotSpot at all three parameter
    // sets — and never advertised. `Security.getProviders(filter)` reads only
    // the registry, so a caller selecting a provider by capability could not
    // see them.
    for (algo, cls) in [
        ("ML-KEM", "com.sun.crypto.provider.ML_KEM_Impls$K"),
        ("ML-KEM-512", "com.sun.crypto.provider.ML_KEM_Impls$K2"),
        ("ML-KEM-768", "com.sun.crypto.provider.ML_KEM_Impls$K3"),
        ("ML-KEM-1024", "com.sun.crypto.provider.ML_KEM_Impls$K5"),
    ] {
        put_service(P, "KEM", algo, cls);
    }
    // Two services this VM has been SERVING all along and never advertised —
    // the `W7-63` half again. `KeyAgreement.DiffieHellman` is the one
    // `jca-provider-population-gap-20260830.md` §4 runs a complete 2048-bit
    // agreement through while noting its whole type was unlisted, and
    // `Signature.NONEwithRSA` has had a `SIG_NONE_RSA` arm since 2026-08-14.
    put_service(
        P,
        "KeyAgreement",
        "DiffieHellman",
        "com.sun.crypto.provider.DHKeyAgreement",
    );
    put_alias(P, "KeyAgreement", "DH", "DiffieHellman");
    put_service(
        P,
        "Signature",
        "NONEwithRSA",
        "com.sun.crypto.provider.RSACipherAdaptor",
    );
}

fn seed_sunjce_pbe_mac_services() {
    const P: &str = "SunJCE";
    // (algorithm spelling, class-name spelling) — the `SHA-512/224` pair
    // differs between the two, as everywhere else in this file.
    const HASHES: &[(&str, &str)] = &[
        ("SHA1", "SHA1"),
        ("SHA224", "SHA224"),
        ("SHA256", "SHA256"),
        ("SHA384", "SHA384"),
        ("SHA512", "SHA512"),
        ("SHA512/224", "SHA512_224"),
        ("SHA512/256", "SHA512_256"),
    ];
    for (algo_hash, class_hash) in HASHES {
        // PKCS#12 `HmacPBESHA*` — `HmacPKCS12PBECore$HmacPKCS12PBE_SHA1`.
        put_service(
            P,
            "Mac",
            &format!("HmacPBE{algo_hash}"),
            &format!("com.sun.crypto.provider.HmacPKCS12PBECore$HmacPKCS12PBE_{class_hash}"),
        );
        // PBMAC1 `PBEWithHmacSHA*` — `PBMAC1Core$HmacSHA1`.
        put_service(
            P,
            "Mac",
            &format!("PBEWithHmac{algo_hash}"),
            &format!("com.sun.crypto.provider.PBMAC1Core$Hmac{class_hash}"),
        );
    }
    for (algo, class) in [
        ("SslMacMD5", "com.sun.crypto.provider.SslMacCore$SslMacMD5"),
        ("SslMacSHA1", "com.sun.crypto.provider.SslMacCore$SslMacSHA1"),
    ] {
        put_service(P, "Mac", algo, class);
    }
}

/// SunJCE's `SecretKeyFactory` table — thirty services, of which this crate
/// advertised NONE.
///
/// Twenty-two of them were being SERVED all along and simply never appeared in
/// `Security.getProvider("SunJCE").getServices()`: measured 2026-09-02, every
/// `PBKDF2With*` and `PBEWith*` name below resolves through
/// `phases_early::pbkdf2_get_instance` and derives bytes identical to
/// HotSpot's. That is precisely the half `W7-63-jca-advertise-vs-serve.md`
/// names in its title — "serves names it never advertised" — and an inventory
/// that cannot see them is wrong about the platform in the safe-looking
/// direction.
///
/// `DES` and `DESede` are the two this engine does NOT compute. They are
/// listed anyway, with the JDK's real class names, because the refusal arm of
/// `pbkdf2_get_instance` now hands those names to the platform's own
/// `DESKeyFactory`/`DESedeKeyFactory` through `jdk_service_class` — so the row
/// is truthful in the only sense that matters: ask for it and you get a
/// working factory.
///
/// Class names are the JDK 25 originals, taken from HotSpot's own
/// `getServices()` enumeration rather than recalled — which matters for the
/// two that are actually instantiated, and for the enumeration diff in the
/// twenty-eight that are not.
fn seed_sunjce_secret_key_factory_services() {
    const P: &str = "SunJCE";
    put_service(
        P,
        "SecretKeyFactory",
        "DES",
        "com.sun.crypto.provider.DESKeyFactory",
    );
    put_service(
        P,
        "SecretKeyFactory",
        "DESede",
        "com.sun.crypto.provider.DESedeKeyFactory",
    );
    // `PBKDF2Core$Hmac*` — the class-name spelling of `SHA-512/224` is
    // `SHA512_224`, the algorithm spelling is `SHA512/224`. Kept as explicit
    // pairs for the same reason the `PBES2Parameters` loop now is: deriving
    // one from the other cost four services there.
    for (algo_hash, class_hash) in [
        ("SHA1", "SHA1"),
        ("SHA224", "SHA224"),
        ("SHA256", "SHA256"),
        ("SHA384", "SHA384"),
        ("SHA512", "SHA512"),
        ("SHA512/224", "SHA512_224"),
        ("SHA512/256", "SHA512_256"),
    ] {
        put_service(
            P,
            "SecretKeyFactory",
            &format!("PBKDF2WithHmac{algo_hash}"),
            &format!("com.sun.crypto.provider.PBKDF2Core$Hmac{class_hash}"),
        );
    }
    // `PBEKeyFactory$*` — here the nested class name KEEPS the `PBEWith`
    // prefix (unlike `PBES2Parameters`, which drops it), so the two families
    // cannot share a formatter.
    for (algo, class_suffix) in [
        ("PBEWithMD5AndDES", "PBEWithMD5AndDES"),
        ("PBEWithMD5AndTripleDES", "PBEWithMD5AndTripleDES"),
        ("PBEWithSHA1AndDESede", "PBEWithSHA1AndDESede"),
        ("PBEWithSHA1AndRC2_40", "PBEWithSHA1AndRC2_40"),
        ("PBEWithSHA1AndRC2_128", "PBEWithSHA1AndRC2_128"),
        ("PBEWithSHA1AndRC4_40", "PBEWithSHA1AndRC4_40"),
        ("PBEWithSHA1AndRC4_128", "PBEWithSHA1AndRC4_128"),
        ("PBEWithHmacSHA1AndAES_128", "PBEWithHmacSHA1AndAES_128"),
        ("PBEWithHmacSHA1AndAES_256", "PBEWithHmacSHA1AndAES_256"),
        ("PBEWithHmacSHA224AndAES_128", "PBEWithHmacSHA224AndAES_128"),
        ("PBEWithHmacSHA224AndAES_256", "PBEWithHmacSHA224AndAES_256"),
        ("PBEWithHmacSHA256AndAES_128", "PBEWithHmacSHA256AndAES_128"),
        ("PBEWithHmacSHA256AndAES_256", "PBEWithHmacSHA256AndAES_256"),
        ("PBEWithHmacSHA384AndAES_128", "PBEWithHmacSHA384AndAES_128"),
        ("PBEWithHmacSHA384AndAES_256", "PBEWithHmacSHA384AndAES_256"),
        ("PBEWithHmacSHA512AndAES_128", "PBEWithHmacSHA512AndAES_128"),
        ("PBEWithHmacSHA512AndAES_256", "PBEWithHmacSHA512AndAES_256"),
        (
            "PBEWithHmacSHA512/224AndAES_128",
            "PBEWithHmacSHA512_224AndAES_128",
        ),
        (
            "PBEWithHmacSHA512/224AndAES_256",
            "PBEWithHmacSHA512_224AndAES_256",
        ),
        (
            "PBEWithHmacSHA512/256AndAES_128",
            "PBEWithHmacSHA512_256AndAES_128",
        ),
        (
            "PBEWithHmacSHA512/256AndAES_256",
            "PBEWithHmacSHA512_256AndAES_256",
        ),
    ] {
        put_service(
            P,
            "SecretKeyFactory",
            algo,
            &format!("com.sun.crypto.provider.PBEKeyFactory${class_suffix}"),
        );
    }
}

/// The two `AlgorithmParameterGenerator` services the JDK ships, and the whole
/// of that engine's story here.
///
/// `AlgorithmParameterGenerator.getInstance` is not intercepted by this crate
/// at all — no native is registered on the class — so it runs ordinary JDK
/// bytecode walking `Provider.getService`, and a service row IS the
/// implementation. No row was seeded for either name, so
/// `AlgorithmParameterGenerator.getInstance("DSA")` raised
/// `NoSuchAlgorithmException` on a VM carrying a working
/// `sun.security.provider.DSAParameterGenerator` in its boot image.
///
/// Both classes are pure Java (`DSAParameterGenerator` is FIPS 186-4 prime
/// generation, `DHParameterGenerator` is safe-prime search over
/// `BigInteger`), take the public no-arg constructor JCA requires, and were
/// confirmed loadable on this VM before the rows were added
/// (`JcaGapSizer --check`).
fn seed_algorithm_parameter_generator_services() {
    put_service(
        "SUN",
        "AlgorithmParameterGenerator",
        "DSA",
        "sun.security.provider.DSAParameterGenerator",
    );
    put_alias("SUN", "AlgorithmParameterGenerator", "1.2.840.10040.4.1", "DSA");
    put_alias(
        "SUN",
        "AlgorithmParameterGenerator",
        "OID.1.2.840.10040.4.1",
        "DSA",
    );
    put_service(
        "SunJCE",
        "AlgorithmParameterGenerator",
        "DiffieHellman",
        "com.sun.crypto.provider.DHParameterGenerator",
    );
    for alias in ["DH", "1.2.840.113549.1.3.1", "OID.1.2.840.113549.1.3.1"] {
        put_alias(
            "SunJCE",
            "AlgorithmParameterGenerator",
            alias,
            "DiffieHellman",
        );
    }
}

/// Mirror the real SunJSSE + SUN provider TLS service tables so that the
/// no-provider `getInstance` search resolves the genuine JDK SPI classes for
/// the TLS engines Tomcat's JSSE connector needs (`KeyManagerFactory`,
/// `TrustManagerFactory`, `SSLContext`) plus `KeyStore` (JKS/PKCS12). Without
/// these, `KeyManagerFactory.getInstance("SunX509")` etc. dead-ended in
/// "no <type> <algo> implementation in any provider" and every HTTPS test
/// aborted. The SPI class names are verified against JDK 25; each has the
/// public no-arg ctor JCA requires, so `build_jca_instance`'s
/// `new_object_initialized(cls, "()V", &[])` runs real provider bytecode.
fn seed_sunjsse_services() {
    const J: &str = "SunJSSE";
    // SunJSSE registers `KeyStore.PKCS12` in its OWN table as well as SUN's,
    // pointing at the plain (non-dual-format) implementation. Applications ask
    // for it by name to read a PKCS#12 written elsewhere with the platform's
    // own store rather than the writer's — `PKCS12StoreTest`'s
    // `checkNoDuplicateOracleTrustedCertAttribute` writes with BouncyCastle and
    // then does exactly that:
    //
    // ```java
    // KeyStore.getInstance("PKCS12", "SunJSSE")
    // ```
    //
    // Without this row the provider resolved, the ownership check refused, and
    // the call was `NoSuchAlgorithmException: no such algorithm: PKCS12 for
    // provider SunJSSE` — a JDK provider being told it does not implement the
    // one KeyStore it is best known for.
    put_service(
        J,
        "KeyStore",
        "PKCS12",
        "sun.security.pkcs12.PKCS12KeyStore",
    );
    // KeyManagerFactory
    put_service(
        J,
        "KeyManagerFactory",
        "SunX509",
        "sun.security.ssl.KeyManagerFactoryImpl$SunX509",
    );
    put_service(
        J,
        "KeyManagerFactory",
        "NewSunX509",
        "sun.security.ssl.KeyManagerFactoryImpl$X509",
    );
    put_alias(J, "KeyManagerFactory", "PKIX", "NewSunX509");
    // TrustManagerFactory
    put_service(
        J,
        "TrustManagerFactory",
        "SunX509",
        "sun.security.ssl.TrustManagerFactoryImpl$SimpleFactory",
    );
    put_service(
        J,
        "TrustManagerFactory",
        "PKIX",
        "sun.security.ssl.TrustManagerFactoryImpl$PKIXFactory",
    );
    put_alias(J, "TrustManagerFactory", "SunPKIX", "PKIX");
    put_alias(J, "TrustManagerFactory", "X509", "PKIX");
    put_alias(J, "TrustManagerFactory", "X.509", "PKIX");
    // SSLContext
    put_service(
        J,
        "SSLContext",
        "TLS",
        "sun.security.ssl.SSLContextImpl$TLSContext",
    );
    put_service(
        J,
        "SSLContext",
        "TLSv1.2",
        "sun.security.ssl.SSLContextImpl$TLS12Context",
    );
    put_service(
        J,
        "SSLContext",
        "TLSv1.3",
        "sun.security.ssl.SSLContextImpl$TLS13Context",
    );
    put_service(
        J,
        "SSLContext",
        "Default",
        "sun.security.ssl.SSLContextImpl$DefaultSSLContext",
    );
    // W3-7: the four entries above were the whole `SSLContext` table, so the
    // chain claimed the platform could not service `TLSv1`, `TLSv1.1`,
    // `SSLv3`, or any DTLS protocol — every one of which SunJSSE really does
    // register on JDK 25. Verified by enumerating
    // `Security.getProvider("SunJSSE").getServices()` on the platform JDK
    // (jdk-25.0.3.9-hotspot): the nine primaries below plus the two
    // `Alg.Alias.SSLContext` entries are exactly what it advertises.
    put_service(
        J,
        "SSLContext",
        "TLSv1",
        "sun.security.ssl.SSLContextImpl$TLS10Context",
    );
    put_service(
        J,
        "SSLContext",
        "TLSv1.1",
        "sun.security.ssl.SSLContextImpl$TLS11Context",
    );
    put_service(
        J,
        "SSLContext",
        "DTLS",
        "sun.security.ssl.SSLContextImpl$DTLSContext",
    );
    put_service(
        J,
        "SSLContext",
        "DTLSv1.0",
        "sun.security.ssl.SSLContextImpl$DTLS10Context",
    );
    put_service(
        J,
        "SSLContext",
        "DTLSv1.2",
        "sun.security.ssl.SSLContextImpl$DTLS12Context",
    );
    put_alias(J, "SSLContext", "SSL", "TLS");
    // `Alg.Alias.SSLContext.SSLv3 -> TLSv1` on the platform JDK — NOT to TLS.
    put_alias(J, "SSLContext", "SSLv3", "TLSv1");
    // KeyStore lives in the SUN provider (JKS/CaseExactJKS/PKCS12/DKS).
    //
    // These five rows are transcribed from the platform JDK, not from memory —
    // every one of them was wrong before, and each wrong in a way that is
    // invisible until something asks the exact question (`KsOwner` probe,
    // measured on jdk-25 on this host):
    //
    // ```text
    // SUN     KeyStore.PKCS12 -> sun.security.pkcs12.PKCS12KeyStore$DualFormatPKCS12
    // SUN     KeyStore.JKS    -> sun.security.provider.JavaKeyStore$DualFormatJKS
    // SUN     KeyStore.DKS    -> sun.security.provider.DomainKeyStore$DKS
    // SunJSSE KeyStore.PKCS12 -> sun.security.pkcs12.PKCS12KeyStore
    // KeyStore.getInstance("PKCS#12") -> KeyStoreException: PKCS#12 not found
    // ```
    //
    // The `DualFormat*` classes are the point of the SUN rows: they are
    // `KeyStoreDelegator`s that sniff the stream and accept EITHER format,
    // which is what makes `keystore.type.compat` mean anything. Naming the
    // plain `PKCS12KeyStore` under SUN quietly removed that, so a JKS stream
    // handed to the platform default failed instead of being detected.
    const S: &str = "SUN";
    put_service(
        S,
        "KeyStore",
        "JKS",
        "sun.security.provider.JavaKeyStore$DualFormatJKS",
    );
    put_service(
        S,
        "KeyStore",
        "CaseExactJKS",
        "sun.security.provider.JavaKeyStore$CaseExactJKS",
    );
    put_service(
        S,
        "KeyStore",
        "PKCS12",
        "sun.security.pkcs12.PKCS12KeyStore$DualFormatPKCS12",
    );
    put_service(
        S,
        "KeyStore",
        "DKS",
        "sun.security.provider.DomainKeyStore$DKS",
    );
    // JCEKS is a SunJCE service, not a SUN one -- `KeyStore.getInstance("JCEKS")`
    // resolves through the JCE provider on HotSpot, and registering it under
    // SUN would put it in the wrong provider for anyone who asks
    // `getProvider().getName()`. Absent entirely until 2026-08-29:
    // `KeyStoreException: JCEKS not found` in both modes, which is the
    // wrong-refuse direction (code that works on every real JDK and dies here).
    //
    // The format is JKS's with a different magic (0xCECECECE) and a stronger
    // key-protection PBE. This VM's JKS path does not decrypt private keys
    // anyway, so what it serves for JCEKS is exactly what it serves for JKS,
    // written under the right magic -- see `keystore::JCEKS_MAGIC`.
    put_service(
        "SunJCE",
        "KeyStore",
        "JCEKS",
        "com.sun.crypto.provider.JceKeyStore",
    );
    // NO `PKCS#12` alias. The JDK does not register one and
    // `KeyStore.getInstance("PKCS#12")` is a `KeyStoreException` there —
    // measured. This VM invented the alias, so a spelling the platform refuses
    // silently succeeded here, which is the wrong-accept direction: code that
    // works on this VM and dies on every real JDK.
    // CertificateFactory X.509 (SUN provider) — needed by the real
    // X509CertImpl/Validator path: the SunX509 KeyManager + PKIX TrustManager
    // build/validate cert chains via `CertificateFactory.getInstance("X.509")`.
    // Without it `getInstance` fell through to "no CertificateFactory X.509
    // implementation in any provider" and aborted SSLContext setup.
    put_service(
        S,
        "CertificateFactory",
        "X.509",
        "sun.security.provider.X509Factory",
    );
    put_alias(S, "CertificateFactory", "X509", "X.509");
    // CertPathValidator / CertPathBuilder PKIX (SUN provider) — needed by the
    // real PKIX TLS trust path and OCSP revocation tests. Without these,
    // `CertPathValidator.getInstance("PKIX")` fell through to "no
    // CertPathValidator PKIX implementation in any provider" and ABORTED the VM
    // (DF06: TestOcspEnabled / TestOcspSoftFail* / TestSecurity2017Ocsp ended as
    // NOSUMMARY). Both Sun SPI classes have the public no-arg ctor JCA requires,
    // so `build_jca_instance`'s `new_object_initialized(cls,"()V")` runs real
    // provider bytecode (verified against JDK 25).
    put_service(
        S,
        "CertPathValidator",
        "PKIX",
        "sun.security.provider.certpath.PKIXCertPathValidator",
    );
    put_service(
        S,
        "CertPathBuilder",
        "PKIX",
        "sun.security.provider.certpath.SunCertPathBuilder",
    );
    // CertStore Collection (SUN provider). `CertPathBuilder.PKIX` above is only
    // half of a path build: PKIX finds intermediates through the `CertStore`s
    // named in its `PKIXBuilderParameters`, and every caller that has the
    // intermediates in hand rather than in an LDAP directory builds that store
    // with `CertStore.getInstance("Collection", new
    // CollectionCertStoreParameters(certs))`. Without the service that call
    // threw `NoSuchAlgorithmException: Collection CertStore not available`, and
    // BouncyCastle wrapped it as the misleading `OCSPException: Error setting up
    // certificate path validation` recorded in the retired
    // `ssl-cert-validation-residuals` write-up — a message that names certificate
    // validation for what is really a missing JCA service.
    //
    // Unlike every other service seeded here, `CertStoreSpi` has NO no-arg
    // constructor: JCA passes the `CertStoreParameters` to a one-argument ctor.
    // `provider_service_new_instance` handles that via
    // `jca_service_ctor_parameter_type`.
    put_service(
        S,
        "CertStore",
        "Collection",
        "sun.security.provider.certpath.CollectionCertStore",
    );
    put_service(
        S,
        "CertStore",
        "com.sun.security.IndexedCollection",
        "sun.security.provider.certpath.IndexedCollectionCertStore",
    );
}

/// The constructor-parameter type a JCA engine's SPI takes, if it takes one.
///
/// Most JCA SPIs are built with a public no-arg constructor, which is what
/// `Provider$Service.newInstance` assumes when it is handed a null
/// `constructorParameter`. A few engines are defined the other way round: the
/// SPI has no no-arg constructor at all and JCA calls a one-argument one. The
/// JDK keeps this in `Provider$Service`'s `knownEngines` table; this mirrors the
/// entries CratonVM actually seeds services for.
///
/// `CertStore` is the one that matters here — `CertStoreSpi(CertStoreParameters)`
/// is its only constructor, so calling `()V` on `CollectionCertStore` cannot
/// work no matter how the service is registered.
fn jca_service_ctor_parameter_type(engine_type: &str) -> Option<&'static str> {
    match engine_type {
        "CertStore" => Some("Ljava/security/cert/CertStoreParameters;"),
        // JEP 478's `KDF`, final in JDK 25 and the second engine of this shape.
        // `KDFSpi`'s only constructor takes `KDFParameters`, and the concrete
        // SunJCE classes declare nothing else — `HKDFKeyDerivation$HKDFSHA256`
        // has a `(KDFParameters)` constructor and NO `()V`.
        "KDF" => Some("Ljavax/crypto/KDFParameters;"),
        _ => None,
    }
}

/// Mirror the JDK's `XMLDSig` provider (`org.jcp.xml.dsig.internal.dom.XMLDSigRI`)
/// JSR-105 service table so `XMLSignatureFactory.getInstance("DOM")` /
/// `KeyInfoFactory.getInstance("DOM")` / `TransformService.getInstance(uri,"DOM")`
/// resolve the real pure-Java DOM SPI classes. The JDK class registers these via
/// `putService(Provider$Service)` (not the legacy `put`/`parseLegacyPut` we
/// intercept), so without seeding, `Provider.getService("XMLSignatureFactory",
/// "DOM")` returns null and `XMLSignatureFactory.getInstance("DOM")` throws
/// `NoSuchMechanismException` (kcfull #01: keycloak SAML XMLSignatureUtil.<clinit>
/// → ExceptionInInitializerError). Class names captured from
/// `javap -p -c org.jcp.xml.dsig.internal.dom.XMLDSigRI$2` on JDK 25; each impl
/// has the public no-arg ctor JCA requires, so `provider_service_new_instance`'s
/// `new_object_initialized(cls,"()V")` runs the real DOM SPI bytecode.
fn seed_xmldsig_services() {
    const X: &str = "XMLDSig";
    put_service(
        X,
        "XMLSignatureFactory",
        "DOM",
        "org.jcp.xml.dsig.internal.dom.DOMXMLSignatureFactory",
    );
    put_service(
        X,
        "KeyInfoFactory",
        "DOM",
        "org.jcp.xml.dsig.internal.dom.DOMKeyInfoFactory",
    );
    // TransformService entries (algorithm == the C14N/transform URI).
    let ts: &[(&str, &str)] = &[
        (
            "http://www.w3.org/TR/2001/REC-xml-c14n-20010315",
            "org.jcp.xml.dsig.internal.dom.DOMCanonicalXMLC14NMethod",
        ),
        (
            "http://www.w3.org/TR/2001/REC-xml-c14n-20010315#WithComments",
            "org.jcp.xml.dsig.internal.dom.DOMCanonicalXMLC14NMethod",
        ),
        (
            "http://www.w3.org/2006/12/xml-c14n11",
            "org.jcp.xml.dsig.internal.dom.DOMCanonicalXMLC14N11Method",
        ),
        (
            "http://www.w3.org/2006/12/xml-c14n11#WithComments",
            "org.jcp.xml.dsig.internal.dom.DOMCanonicalXMLC14N11Method",
        ),
        (
            "http://www.w3.org/2001/10/xml-exc-c14n#",
            "org.jcp.xml.dsig.internal.dom.DOMExcC14NMethod",
        ),
        (
            "http://www.w3.org/2001/10/xml-exc-c14n#WithComments",
            "org.jcp.xml.dsig.internal.dom.DOMExcC14NMethod",
        ),
        (
            "http://www.w3.org/2000/09/xmldsig#base64",
            "org.jcp.xml.dsig.internal.dom.DOMBase64Transform",
        ),
        (
            "http://www.w3.org/2000/09/xmldsig#enveloped-signature",
            "org.jcp.xml.dsig.internal.dom.DOMEnvelopedTransform",
        ),
        (
            "http://www.w3.org/2002/06/xmldsig-filter2",
            "org.jcp.xml.dsig.internal.dom.DOMXPathFilter2Transform",
        ),
        (
            "http://www.w3.org/TR/1999/REC-xpath-19991116",
            "org.jcp.xml.dsig.internal.dom.DOMXPathTransform",
        ),
        (
            "http://www.w3.org/TR/1999/REC-xslt-19991116",
            "org.jcp.xml.dsig.internal.dom.DOMXSLTTransform",
        ),
    ];
    for (uri, cls) in ts {
        put_service(X, "TransformService", uri, cls);
    }
}

/// Internal API: register an alias (Alg.Alias.<type>.<alias> → canonical).
///
/// The KEY is normalised (lookups are case-insensitive, per `Provider$ServiceKey`);
/// the VALUE keeps the provider's own spelling. `get_service_entry` re-normalises
/// it for the service-map lookup, and `canonical_service_algorithm` hands the raw
/// form to the native engines — which is the form a caller-facing `getAlgorithm()`
/// may echo, and the form `Alg.Alias.KeyFactory.DH = DiffieHellman` has to keep to
/// stay a legible answer rather than `DIFFIEHELLMAN`.
fn put_alias(provider: &str, type_str: &str, alias: &str, canonical: &str) {
    let type_n = normalize_engine(type_str);
    let alias_n = normalize_algo(alias);
    aliases().lock().insert(
        (provider.to_string(), type_n, alias_n),
        AliasEntry {
            canonical: canonical.trim().to_string(),
            type_str: type_str.trim().to_string(),
            alias: alias.trim().to_string(),
        },
    );
}

/// Apply a single `put(key, value)` operation against the provider's
/// service map.  Keys that don't parse as `Type.Algorithm` /
/// `Alg.Alias.Type.Alias` / `Type.Algorithm Attr` are silently ignored
/// — they'd land in the inherited `Hashtable` slot in the real JDK and
/// never participate in service resolution.
///
/// Returns `true` when the put resulted in a service-map mutation,
/// `false` when the key was non-service-shaped.
fn apply_legacy_put(provider: &str, key: &str, value: &str) -> bool {
    let parsed = match parse_legacy_key(key) {
        Some(p) => p,
        None => return false,
    };
    match parsed.0.as_str() {
        "primary" => {
            put_service(provider, &parsed.1, &parsed.2, value);
            true
        }
        "alias" => {
            // Alias maps: parsed.2 is the alias name, value is canonical algo.
            put_alias(provider, &parsed.1, &parsed.2, value);
            true
        }
        "attr" => {
            // Attribute on a service. These used to be DROPPED, on the reading
            // that only `Service.supportsParameter` consumes them — but
            // `Service.getAttribute` is public and applications read it
            // directly: bc-java's `ECAlgorithmParametersTest` asks
            // `getService("AlgorithmParameters", "EC").getAttribute("SupportedCurves")`
            // and NPE'd on the null, and `BouncyCastleProviderTest` asserts an
            // attribute is visible through an ALIAS as well.
            if let Some(attr) = parsed.3.as_deref() {
                put_service_attribute(provider, &parsed.1, &parsed.2, attr, value);
            }
            true
        }
        _ => false,
    }
}

/// Discover the provider name attached to a `Provider` receiver.  Used
/// by `Provider.put` / `parseLegacyPut` to key the global service map.
/// Falls back to `"<unknown>"` if the receiver has no name (which would
/// only happen for synthetic test fixtures).
pub(crate) fn provider_name_of(ctx: &dyn NativeContext, prov: ObjectRef) -> String {
    if let Some((name, _)) = read_provider_name_version(ctx, prov) {
        if !name.is_empty() {
            return name;
        }
    }
    "<unknown>".to_string()
}

/// `Provider.put(Object key, Object value)` native — accepts any
/// `(Object, Object)` pair, but only `(String, String)` participates
/// in service-map population.  Other shapes (Object value used as
/// attribute holder) silently no-op on the service side; the value is
/// preserved on the inherited Hashtable in the real-JDK class so the
/// JVM doesn't lose state.
///
/// Returns the previous value the same way `Hashtable.put` does — we
/// return null because there is no per-key prior-value tracking on our
/// shim, which matches the behaviour any caller sees on a fresh
/// Provider's first put.
fn provider_put_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let value = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        // Non-string value (rare — BC sometimes puts an attribute Map).
        // Stringify as empty so `apply_legacy_put` still parses the key
        // shape and stores an entry with an empty class_name.
        _ => String::new(),
    };
    let provider_name = provider_name_of(ctx, this);
    provider_properties()
        .lock()
        .insert((provider_name.clone(), key.clone()), value.clone());
    let ihash = ctx.identity_hash_code(this) as i64;
    provider_instance_keys().lock().insert((ihash, key.clone()));
    apply_legacy_put(&provider_name, &key, &value);
    // Hashtable.put contract: return previous value (null on first put).
    Ok(Some(Value::Object(None)))
}

/// `Provider.parseLegacyPut(String name, String value)` native — the
/// JDK's package-private helper that BouncyCastle's `addAlgorithm`
/// chain calls into.  Identical population semantics to `put` modulo
/// the return type (void).
///
/// The descriptor used here matches the OpenJDK 21+ source — a 2-arg
/// instance method.  If a future JDK version moves this to a
/// `Properties#put` override, the put native above already handles
/// that path.
fn provider_parse_legacy_put_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    let value = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let provider_name = provider_name_of(ctx, this);
    provider_properties()
        .lock()
        .insert((provider_name.clone(), name.clone()), value.clone());
    let ihash = ctx.identity_hash_code(this) as i64;
    provider_instance_keys()
        .lock()
        .insert((ihash, name.clone()));
    apply_legacy_put(&provider_name, &name, &value);
    Ok(None)
}

/// `java.security.Provider.putService(Provider$Service)` native.
///
/// Modern providers (BouncyCastle's `Mappings.configure()` chain among
/// them — see e.g. `GOST3411$Mappings`) register services by
/// constructing a real `Provider.Service` object and calling this
/// method directly, bypassing the legacy `put`/`parseLegacyPut(String,
/// String)` surface entirely. Before this native existed, the call fell
/// through to real inherited `Provider.putService` bytecode, which
/// reads/writes the real `legacyMap`/`serviceMap` fields — but those are
/// never initialized on our synthetic `Provider` instances (the real
/// `Provider` constructor never runs for them; see `seed_sunec_services`
/// doc comment above for the same observation re: SunEC). Operating on
/// those uninitialized maps made the real bytecode's own duplicate-key
/// bookkeeping misfire on ordinary re-registration, throwing
/// `IllegalStateException: duplicate provider key (...) found` wrapped
/// in an `InternalError` — even though the exact same provider code
/// runs fine on real HotSpot, whose `Provider` maps are properly
/// constructed.
///
/// Fix: read the `Service`'s `type`/`algorithm`/`className` fields
/// directly and route them through the same `put_service` side-table
/// `put`/`parseLegacyPut` already use — which is a plain `HashMap`
/// `insert` (last-write-wins, never throws on a duplicate key), matching
/// real `Provider.putService`'s actual observable behaviour (replacing
/// any previous registration for the same provider/type/algorithm).
fn provider_put_service_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let service = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => return Ok(None),
    };
    let type_str = provider_service_string_field(ctx, service, "type", 0).unwrap_or_default();
    let algorithm = provider_service_string_field(ctx, service, "algorithm", 1).unwrap_or_default();
    let class_name = provider_service_string_field(ctx, service, "className", 3)
        .or_else(|| {
            let ih = ctx.identity_hash_code(service) as i64;
            service_classname_table().lock().get(&ih).cloned()
        })
        .unwrap_or_default();
    if type_str.is_empty() || algorithm.is_empty() {
        // Malformed/partial Service — nothing sensible to register.
        return Ok(None);
    }
    let provider_name = provider_name_of(ctx, this);
    put_service(&provider_name, &type_str, &algorithm, &class_name);

    if !class_name.is_empty() {
        let ih = ctx.identity_hash_code(service) as i64;
        service_classname_table()
            .lock()
            .insert(ih, class_name.clone());
    }

    // Mirror the legacy `put` path's raw-property bookkeeping so
    // `getProperty`/`containsKey` on the equivalent legacy key also see
    // this registration (some providers query back via either surface).
    let legacy_key = format!("{type_str}.{algorithm}");
    provider_properties()
        .lock()
        .insert((provider_name, legacy_key.clone()), class_name);
    let ihash = ctx.identity_hash_code(this) as i64;
    provider_instance_keys().lock().insert((ihash, legacy_key));
    Ok(None)
}

/// `java.security.Provider.getProperty(String key)` — return the value recorded
/// for `(this-provider-name, key)` by `put`/`parseLegacyPut`, or null. Bypasses
/// the real `Provider.getProperty` (which calls `checkInitialized()` and throws
/// `IllegalStateException` on our synthetic providers whose `initialized` flag
/// is never set), and returns the alias/property values BouncyCastle's
/// `X509SignatureUtil.lookupAlg` etc. read back via `getProperty`.
fn provider_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let pname = provider_name_of(ctx, this);
    let val = provider_properties()
        .lock()
        .get(&(pname.clone(), key.clone()))
        .cloned()
        // ...and the seeded registry behind it, which is where every
        // `Type.Algorithm` and `Alg.Alias.Type.Alias` row this provider was
        // built with actually lives. BouncyCastle reads aliases back through
        // exactly this call (`X509SignatureUtil.lookupAlg`).
        .or_else(|| {
            projected_registry_rows(&pname)
                .into_iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v)
        });
    match val {
        Some(v) => {
            let s = ctx.create_string(&v);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `java.security.Provider.containsKey(Object)` — BouncyCastle FIPS checks
/// primary provider keys this way before registering aliases. Our `put` native
/// records raw properties in a side table instead of the inherited Hashtable, so
/// the inherited bytecode would falsely report "missing" for keys just put.
///
/// Scoped to the RECEIVER's own identity hash (`provider_instance_keys()`),
/// not the shared by-name `provider_properties()` table. `addAlgorithm`
/// (BouncyCastle's `ConfigurableProvider` implementation) calls
/// `containsKey(key)` first and throws `IllegalStateException: duplicate
/// provider key` if it's already true — real HotSpot never trips this for a
/// second, independent `new BouncyCastleProvider()` because each instance's
/// backing map starts empty. Reading the name-shared table here would make
/// one BC instance's registrations spuriously "poison" every other
/// same-named instance's `addAlgorithm` calls, which is exactly what
/// happened when `JcaX509CertificateConverter`'s internal `BCJcaJceHelper`
/// constructs its own separate `BouncyCastleProvider` on top of one already
/// cached elsewhere (see `ServerHttpsRequestIntegrationTests`'s self-signed
/// cert generation path, which does exactly this).
fn provider_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    // Answers from the same rows `keySet`/`size`/`entrySet` project — see
    // `provider_map_rows`, which is where the scoping rule is explained.
    //
    // Instance scope is LOAD-BEARING and not an accident. BouncyCastle's
    // `BouncyCastleProvider.addAlgorithm` is:
    //
    // ```java
    // if (containsKey(key)) {
    //     throw new IllegalStateException("duplicate provider key (" + key + ") found");
    // }
    // ```
    //
    // so a process-global answer makes the SECOND `new BouncyCastleProvider()`
    // throw on its first registration. On HotSpot each provider instance owns
    // its own map and sees none of the first one's keys. Making this
    // name-keyed to match `get` was tried and did exactly that:
    // `cannot create instance of ...GOST3411$Mappings : duplicate provider key
    // (MessageDigest.GOST3411) found`.
    let found = provider_row_present(ctx, this, &key);
    Ok(Some(Value::Int(if found { 1 } else { 0 })))
}

/// `java.security.Provider.get(Object)` — keep raw property reads consistent
/// with `containsKey` for provider implementations that consult the inherited
/// Map surface directly instead of `getProperty(String)`.
fn provider_get_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let pname = provider_name_of(ctx, this);
    match provider_properties().lock().get(&(pname, key)).cloned() {
        Some(v) => {
            let s = ctx.create_string(&v);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// GC-stable side table mapping a synthetic `Provider$Service` id to its
/// implementation class name. The `Provider$Service` synthetic's object slots
/// hold `String` references that are NOT reliably traced/forwarded by the moving
/// collector (the class is allocated as a synthetic stub whose raw slots the GC
/// does not treat as declared reference fields), so a className kept only in an
/// object slot can go stale across the `getService` -> `newInstance` window and
/// read back empty. Keying on an integer id (primitives are never relocated)
/// makes className retrieval robust.
/// `Provider$Service` identity hash -> its attribute rows.
///
/// Same reasoning as [`service_classname_table`]: the synthetic
/// `Provider$Service`'s own reference slots are not a reliable place to hang
/// state, and the JDK's `getAttribute` reads a map keyed by its private
/// `UString` wrapper, which we cannot populate from here.
fn service_attributes_table(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i64, Vec<(String, String)>>> {
    static T: std::sync::OnceLock<
        parking_lot::Mutex<rustc_hash::FxHashMap<i64, Vec<(String, String)>>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(Default::default)
}

fn service_classname_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i64, String>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i64, String>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Allocate a `Provider$Service` synthetic populated from a stored
/// `ServiceEntry`.  Used by `provider_get_service_native` and the
/// `Cipher.getInstance(algo, providerName)` resolution path.
fn make_service(
    ctx: &mut dyn NativeContext,
    entry: &ServiceEntry,
    prov: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let prov_pin = ctx.pin_native_root(prov);
    let svc0 = try_alloc_concurrent_synthetic(ctx, "java/security/Provider$Service", 7)?;
    let svc_pin = ctx.pin_native_root(svc0);
    let type_s0 = ctx.create_string(&entry.type_str);
    let type_pin = ctx.pin_native_root(type_s0);
    let algo_s0 = ctx.create_string(&entry.algorithm);
    let algo_pin = ctx.pin_native_root(algo_s0);
    let class_s0 = ctx.create_string(&entry.class_name);
    let class_pin = ctx.pin_native_root(class_s0);

    let prov = ctx.read_native_pin(prov_pin, prov);
    let svc = ctx.read_native_pin(svc_pin, svc0);
    let type_s = ctx.read_native_pin(type_pin, type_s0);
    let algo_s = ctx.read_native_pin(algo_pin, algo_s0);
    let class_s = ctx.read_native_pin(class_pin, class_s0);

    let aliases = empty_collection_value(
        ctx,
        "emptyList",
        "()Ljava/util/List;",
        "java/util/ArrayList",
    )?;
    let svc = ctx.read_native_pin(svc_pin, svc0);
    ctx.set_field_by_name(svc, "aliases", aliases);
    let attributes =
        empty_collection_value(ctx, "emptyMap", "()Ljava/util/Map;", "java/util/HashMap")?;
    let svc = ctx.read_native_pin(svc_pin, svc0);
    ctx.set_field_by_name(svc, "attributes", attributes);

    // Real-JDK path — write by field name.
    ctx.set_field_by_name(svc, "provider", Value::Object(Some(prov)));
    ctx.set_field_by_name(svc, "type", Value::Object(Some(type_s)));
    ctx.set_field_by_name(svc, "algorithm", Value::Object(Some(algo_s)));
    ctx.set_field_by_name(svc, "className", Value::Object(Some(class_s)));

    // Synthetic-mode mirror — getType=0, getAlgorithm=1, getProvider=2
    // (matches the layout in `phases_early::register_phase53_security`).
    // Skipped when the named writes above landed: see
    // `service_has_named_layout` for what the mirror did to a real
    // `Provider$Service`, and how `CertStore.getInstance` exposed it.
    if !service_has_named_layout(ctx, svc, prov) {
        ctx.set_field(svc, 0, Value::Object(Some(type_s)));
        ctx.set_field(svc, 1, Value::Object(Some(algo_s)));
        ctx.set_field(svc, 2, Value::Object(Some(prov)));
        // Slot 3 reserved for className so the new `Service.getClassName`
        // accessor (registered below) returns the right string.
        ctx.set_field(svc, 3, Value::Object(Some(class_s)));
    }
    // GC-stable className: key the side table on the service's identity hash
    // (stored in the object header, preserved across moving-GC relocation) so
    // `newInstance` retrieves the className without depending on object slots
    // — the synthetic `Provider$Service`'s reference slots are neither reliably
    // forwarded by the collector nor int-writable (writes to ref slots coerce
    // to null), making slot-based storage unreliable.
    let ih = ctx.identity_hash_code(svc) as i64;
    service_classname_table()
        .lock()
        .insert(ih, entry.class_name.clone());
    service_attributes_table()
        .lock()
        .insert(ih, entry.attributes.clone());
    ctx.unpin_native_roots(prov_pin);
    Ok(svc)
}

/// `Provider.getService(String type, String algorithm)` native —
/// resolves a service entry from the per-provider map populated via
/// `put` / `parseLegacyPut`.  Returns null if the provider has no
/// entry for that `(type, algorithm)` pair (matches the JDK contract;
/// `Cipher.getInstance` then throws `NoSuchAlgorithmException`).
fn provider_get_service_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let type_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let algo = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let prov_name = provider_name_of(ctx, this);
    match get_service_entry(&prov_name, &type_str, &algo) {
        Some(entry) => {
            let svc = make_service(ctx, &entry, this)?;
            Ok(Some(Value::Object(Some(svc))))
        }
        None => {
            // C19: when a placeholder ("unbacked") provider is asked
            // for a Service, log a debug breadcrumb. This is the most
            // common cause of the "Cipher.getInstance throws deep
            // NoSuchAlgorithmException" puzzle — the caller asked SUN /
            // SunMSCAPI / SunPKCS11 / ... for an algorithm that we've
            // never registered because the provider is purely an
            // entry in the chain for legacy-app compatibility.
            //
            // Only fire when both type and algo are non-empty: empty
            // strings reach here from `args` decoding fallbacks (null
            // String arg etc.) and aren't a useful diagnostic signal.
            if !type_str.is_empty() && !algo.is_empty() && is_unbacked_provider(&prov_name) {
                tracing::debug!(
                    provider = %prov_name,
                    service_type = %type_str,
                    algorithm = %algo,
                    "Provider.getService on unbacked provider — no Service map registered \
                     (placeholder entry for legacy app compatibility); lookup returns null"
                );
            }
            Ok(Some(Value::Object(None)))
        }
    }
}

/// `Provider.getServices()` native: expose the service side table used by
/// `getService(type, algorithm)`. The early bootstrap fallback returns an empty
/// set; BouncyCastle JSSE needs the populated set while constructing the FIPS
/// provider so it can discover TLS key/trust manager algorithms.
/// Every `(key, value)` this provider's Map surface must show.
///
/// `java.security.Provider` is a `Properties`, and applications read it as one
/// — `for (Object k : provider.keySet())` is an ordinary idiom, and
/// BouncyCastle's own `BouncyCastleProviderTest.testRegisteredClasses` does
/// exactly that. Before this, the Map surface had **three** disagreeing views:
///
/// | | HotSpot | CratonVM (before) |
/// |---|---|---|
/// | `keySet().size()` | 5153 | 4 |
/// | `get("Provider.id name")` | `BC` | `null` |
/// | `containsKey(k)` after `put(k, v)` | true | true |
/// | `size()` after that `put` | 5 | **4** |
///
/// `put`, `parseLegacyPut` and `putService` all record into
/// `provider_properties` (name-keyed, with the value) and
/// `provider_instance_keys` (identity-keyed), and `get`/`containsKey` read
/// those — but `size`/`keySet`/`entrySet`/`values` were never registered at
/// all, so they fell through to the inherited `Properties` map, which only
/// `putId`'s four `Provider.id *` rows had ever reached. Three stores, three
/// answers, no error anywhere.
///
/// Everything now projects the two tables every writer maintains together:
/// `provider_instance_keys` for WHICH keys this instance has, and
/// `provider_properties` for their values.
///
/// Scoped to the INSTANCE, because that is what a real `Provider` is — its own
/// `Properties` map — and because BouncyCastle depends on it: `addAlgorithm`
/// refuses a key `containsKey` already reports, so a process-global answer
/// makes the second `new BouncyCastleProvider()` throw. See
/// `provider_contains_key`.
///
/// The fallback covers the VM's own stand-ins: `make_provider` mints a fresh
/// synthetic `Provider` per call, which never ran a `put` and so owns no
/// instance keys, but must still show the rows registered under its name.
/// An instance that has put SOMETHING is authoritative about its own contents;
/// only one that has put NOTHING defers to its name.
///
/// Sorted so `keySet`, `values` and `entrySet` agree with each other
/// positionally and the order is stable between calls. HotSpot's is a hash
/// order and is unspecified, so a defined order is not a divergence.
/// A provider's SEEDED registry, rendered as the legacy property rows a real
/// `Provider` carries in its inherited `Properties` table.
///
/// `put_service` / `put_service_attribute` / `put_alias` capture into the
/// structured maps `getInstance` reads, and nothing ever mirrored them into the
/// property view. So `Security.getProvider("SUN").keySet()` answered EMPTY,
/// where HotSpot 25 answers 257 keys — 68 services, 86 attributes, 103 aliases
/// — and every introspection path built on it (`keySet`, `entrySet`, `values`,
/// `keys`, `elements`, `size`, `isEmpty`, `getProperty`) was silently wrong
/// rather than merely incomplete.
///
/// Rendered on demand from the structured maps rather than duplicated at
/// `put` time, so there is one source of truth and no way for the two to
/// disagree. The three key shapes are HotSpot's own, measured:
///
/// ```text
/// MessageDigest.SHA-256                       -> sun.security.provider.SHA2$SHA256
/// MessageDigest.SHA-256 ImplementedIn         -> Software
/// Alg.Alias.MessageDigest.SHA256              -> SHA-256
/// ```
///
/// A service with an empty `class_name` is an alias-only artefact of the
/// legacy `put` flow and is NOT rendered: it is not a row any provider
/// advertises.
fn projected_registry_rows(name: &str) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    {
        let services = services().lock();
        if let Some(map) = services.get(name) {
            for entry in map.values() {
                if !entry.class_name.trim().is_empty() {
                    rows.push((
                        format!("{}.{}", entry.type_str, entry.algorithm),
                        entry.class_name.clone(),
                    ));
                }
                for (attr, value) in &entry.attributes {
                    rows.push((
                        format!("{}.{} {}", entry.type_str, entry.algorithm, attr),
                        value.clone(),
                    ));
                }
            }
        }
    }
    {
        let aliases = aliases().lock();
        for ((provider, _, _), entry) in aliases.iter() {
            if provider == name {
                rows.push((
                    format!("Alg.Alias.{}.{}", entry.type_str, entry.alias),
                    entry.canonical.clone(),
                ));
            }
        }
    }
    rows
}

fn provider_map_rows(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<(String, String)> {
    let name = provider_name_of(ctx, this);
    let ihash = ctx.identity_hash_code(this) as i64;
    let own: Vec<String> = provider_instance_keys()
        .lock()
        .iter()
        .filter(|(instance, _)| *instance == ihash)
        .map(|(_, key)| key.clone())
        .collect();
    let properties = provider_properties().lock();
    let mut rows: Vec<(String, String)> = if own.is_empty() {
        // An explicit `put` WINS over the projection for the same key: the
        // property table is what a caller last wrote, and the registry is what
        // was seeded. Collected properties-first, then deduplicated by key
        // after the sort below, so the put survives.
        let mut merged: Vec<(String, String)> = properties
            .iter()
            .filter(|((provider, _), _)| provider == &name)
            .map(|((_, key), value)| (key.clone(), value.clone()))
            .collect();
        let put_keys: std::collections::HashSet<String> =
            merged.iter().map(|(k, _)| k.clone()).collect();
        merged.extend(
            projected_registry_rows(&name)
                .into_iter()
                .filter(|(k, _)| !put_keys.contains(k)),
        );
        merged
    } else {
        own.into_iter()
            .map(|key| {
                let value = properties
                    .get(&(name.clone(), key.clone()))
                    .cloned()
                    .unwrap_or_default();
                (key, value)
            })
            .collect()
    };
    drop(properties);
    rows.sort_unstable();
    rows
}

/// Is `key` one of this provider's rows, under the same scoping rule
/// [`provider_map_rows`] projects? Answered without materialising every row.
fn provider_row_present(ctx: &mut dyn NativeContext, this: ObjectRef, key: &str) -> bool {
    let ihash = ctx.identity_hash_code(this) as i64;
    let (has_own_key, has_any_own) = {
        let instance_keys = provider_instance_keys().lock();
        (
            instance_keys.contains(&(ihash, key.to_string())),
            instance_keys.iter().any(|(instance, _)| *instance == ihash),
        )
    };
    if has_any_own {
        return has_own_key;
    }
    let name = provider_name_of(ctx, this);
    provider_properties()
        .lock()
        .contains_key(&(name, key.to_string()))
}

/// Build an unmodifiable `Set` of the strings produced by `pick`.
///
/// Each string is pinned across the `add` that follows it: the set, the string
/// and every node allocated on the way can trigger a moving collection, and a
/// raw `ObjectRef` held only in this Rust frame would be stale afterwards.
/// This is the same discipline `provider_get_services_native` uses.
fn provider_string_set(
    ctx: &mut dyn NativeContext,
    rows: &[(String, String)],
    pick: fn(&(String, String)) -> &String,
) -> Result<ObjectRef, MethodCallFailed> {
    let set = cratonvm_native_collections::make_hashset_with_elements(ctx, &[])?;
    let set_pin = ctx.pin_native_root(set);
    for row in rows {
        let element = ctx.create_string(pick(row));
        let element_pin = ctx.pin_native_root(element);
        let set_now = ctx.read_native_pin(set_pin, set);
        let element_now = ctx.read_native_pin(element_pin, element);
        let added = ctx.invoke(
            "java/util/HashSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[
                Value::Object(Some(set_now)),
                Value::Object(Some(element_now)),
            ],
        );
        ctx.unpin_native_roots(element_pin);
        if let Err(e) = added {
            ctx.unpin_native_roots(set_pin);
            return Err(e);
        }
    }
    let set = ctx.read_native_pin(set_pin, set);
    let view = wrap_unmodifiable(ctx, set);
    ctx.unpin_native_roots(set_pin);
    Ok(view)
}

/// `Provider.size()` — the number of registered rows, not the four the
/// inherited map happened to hold.
fn provider_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let n = provider_map_rows(ctx, this).len();
    Ok(Some(Value::Int(n as i32)))
}

fn provider_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let empty = provider_map_rows(ctx, this).is_empty();
    Ok(Some(Value::Int(i32::from(empty))))
}

/// `Provider.keySet()` — unmodifiable, as the real `Provider` returns.
fn provider_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let rows = provider_map_rows(ctx, this);
    let set = provider_string_set(ctx, &rows, |(key, _)| key)?;
    Ok(Some(Value::Object(Some(set))))
}

/// `Properties.stringPropertyNames()` — every row here has a String key and a
/// String value, so it is the same set as `keySet`.
fn provider_string_property_names(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    provider_key_set(ctx, args)
}

/// `Provider.values()` — a `Collection`, and duplicates are meaningful here
/// (many algorithms map to one implementation class), so this is a List and
/// NOT a Set.
fn provider_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let rows = provider_map_rows(ctx, this);
    let list = ctx.new_object_initialized("java/util/ArrayList", "()V", &[])?;
    let list = match list {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let list_pin = ctx.pin_native_root(list);
    for (_, value) in &rows {
        let element = ctx.create_string(value);
        let element_pin = ctx.pin_native_root(element);
        let list_now = ctx.read_native_pin(list_pin, list);
        let element_now = ctx.read_native_pin(element_pin, element);
        let added = ctx.invoke(
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[
                Value::Object(Some(list_now)),
                Value::Object(Some(element_now)),
            ],
        );
        ctx.unpin_native_roots(element_pin);
        if let Err(e) = added {
            ctx.unpin_native_roots(list_pin);
            return Err(e);
        }
    }
    let list = ctx.read_native_pin(list_pin, list);
    let view = match ctx.invoke(
        "java/util/Collections",
        "unmodifiableCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        &[Value::Object(Some(list))],
    ) {
        Ok(Some(Value::Object(Some(v)))) => v,
        _ => list,
    };
    ctx.unpin_native_roots(list_pin);
    Ok(Some(Value::Object(Some(view))))
}

/// `Provider.entrySet()` — unmodifiable, of real `Map.Entry` objects, so
/// `entry.getKey()` / `entry.getValue()` work on the result.
fn provider_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let rows = provider_map_rows(ctx, this);
    let set = cratonvm_native_collections::make_hashset_with_elements(ctx, &[])?;
    let set_pin = ctx.pin_native_root(set);
    for (key, value) in &rows {
        let k = ctx.create_string(key);
        let k_pin = ctx.pin_native_root(k);
        let v = ctx.create_string(value);
        let v_pin = ctx.pin_native_root(v);
        let k_now = ctx.read_native_pin(k_pin, k);
        let v_now = ctx.read_native_pin(v_pin, v);
        let entry = ctx.new_object_initialized(
            "java/util/AbstractMap$SimpleEntry",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[Value::Object(Some(k_now)), Value::Object(Some(v_now))],
        );
        ctx.unpin_native_roots(v_pin);
        ctx.unpin_native_roots(k_pin);
        let entry = match entry {
            Ok(Some(Value::Object(Some(o)))) => o,
            Ok(_) => continue,
            Err(e) => {
                ctx.unpin_native_roots(set_pin);
                return Err(e);
            }
        };
        let entry_pin = ctx.pin_native_root(entry);
        let set_now = ctx.read_native_pin(set_pin, set);
        let entry_now = ctx.read_native_pin(entry_pin, entry);
        let added = ctx.invoke(
            "java/util/HashSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(set_now)), Value::Object(Some(entry_now))],
        );
        ctx.unpin_native_roots(entry_pin);
        if let Err(e) = added {
            ctx.unpin_native_roots(set_pin);
            return Err(e);
        }
    }
    let set = ctx.read_native_pin(set_pin, set);
    let view = wrap_unmodifiable(ctx, set);
    ctx.unpin_native_roots(set_pin);
    Ok(Some(Value::Object(Some(view))))
}

/// `Hashtable.keys()` / `Hashtable.elements()` — the Enumeration surface, built
/// from a real `Vector` so the returned object is a genuine JDK Enumeration
/// rather than a synthetic stand-in.
fn provider_enumeration_of(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    values_not_keys: bool,
) -> MethodCallResult {
    let rows = provider_map_rows(ctx, this);
    let vector = ctx.new_object_initialized("java/util/Vector", "()V", &[])?;
    let vector = match vector {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let vector_pin = ctx.pin_native_root(vector);
    for (key, value) in &rows {
        let element = ctx.create_string(if values_not_keys { value } else { key });
        let element_pin = ctx.pin_native_root(element);
        let vector_now = ctx.read_native_pin(vector_pin, vector);
        let element_now = ctx.read_native_pin(element_pin, element);
        let added = ctx.invoke(
            "java/util/Vector",
            "add",
            "(Ljava/lang/Object;)Z",
            &[
                Value::Object(Some(vector_now)),
                Value::Object(Some(element_now)),
            ],
        );
        ctx.unpin_native_roots(element_pin);
        if let Err(e) = added {
            ctx.unpin_native_roots(vector_pin);
            return Err(e);
        }
    }
    let vector = ctx.read_native_pin(vector_pin, vector);
    let out = ctx.invoke(
        "java/util/Vector",
        "elements",
        "()Ljava/util/Enumeration;",
        &[Value::Object(Some(vector))],
    );
    ctx.unpin_native_roots(vector_pin);
    out
}

fn provider_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    provider_enumeration_of(ctx, this, false)
}

fn provider_elements(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    provider_enumeration_of(ctx, this, true)
}

/// `Provider.putId()` — the four `Provider.id *` rows.
///
/// The real one writes them with `super.put`, which is an `invokespecial` on
/// `Properties` and therefore bypasses the `Provider.put` native entirely. That
/// is why those four rows were the ONLY thing the inherited map ever held, and
/// why `get("Provider.id name")` answered null while `keySet().size()` was
/// exactly 4. Recording them the way every other writer does puts them in the
/// one store, so all the views above show them.
///
/// The real `super.put` calls are deliberately NOT reproduced. Routing them
/// back through `Properties.put` threw inside `Provider.<init>` — the synthetic
/// `Provider` layout this VM allocates does not carry a usable `Properties`
/// backing map — and with every Map view registered above there is no longer a
/// reader for the inherited map. One store, and it is this one.
fn provider_put_id(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = provider_name_of(ctx, this);
    let version = match ctx.get_field_by_name(this, "versionStr") {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        _ => String::new(),
    };
    let info = match ctx.get_field_by_name(this, "info") {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        _ => String::new(),
    };
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "java.security.Provider".to_string())
        .replace('/', ".");

    let rows = [
        ("Provider.id name", name.clone()),
        ("Provider.id version", version),
        ("Provider.id info", info),
        ("Provider.id className", class_name),
    ];
    let ihash = ctx.identity_hash_code(this) as i64;
    for (key, value) in &rows {
        provider_properties()
            .lock()
            .insert((name.clone(), (*key).to_string()), value.clone());
        provider_instance_keys()
            .lock()
            .insert((ihash, (*key).to_string()));
    }

    let _ = ihash;
    Ok(None)
}

fn provider_get_services_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let prov_name = provider_name_of(ctx, this);
    let entries: Vec<ServiceEntry> = services()
        .lock()
        .get(&prov_name)
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default();

    let this_pin = ctx.pin_native_root(this);
    let set = cratonvm_native_collections::make_hashset_with_elements(ctx, &[])?;
    let set_pin = ctx.pin_native_root(set);
    for entry in entries {
        let prov = ctx.read_native_pin(this_pin, this);
        let svc = make_service(ctx, &entry, prov)?;
        let svc_pin = ctx.pin_native_root(svc);
        let set = ctx.read_native_pin(set_pin, set);
        let svc = ctx.read_native_pin(svc_pin, svc);
        let add_result = ctx.invoke(
            "java/util/HashSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(set)), Value::Object(Some(svc))],
        );
        ctx.unpin_native_roots(svc_pin);
        if let Err(e) = add_result {
            ctx.unpin_native_roots(set_pin);
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    let set = ctx.read_native_pin(set_pin, set);
    // As in `security_get_algorithms`: wrap while still pinned, then unpin
    // both. HotSpot answers `Collections$UnmodifiableSet` here too (n=65 for
    // SUN, measured) and `add(null)` raises `UnsupportedOperationException`.
    let view = wrap_unmodifiable(ctx, set);
    ctx.unpin_native_roots(set_pin);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(view))))
}

fn provider_service_string_field(
    ctx: &mut dyn NativeContext,
    service: ObjectRef,
    field: &str,
    slot: usize,
) -> Option<String> {
    fn non_empty_string(ctx: &mut dyn NativeContext, value: Value) -> Option<String> {
        match value {
            Value::Object(Some(s)) => ctx.read_string(s).filter(|s| !s.is_empty()),
            _ => None,
        }
    }

    let by_name = ctx.get_field_by_name(service, field);
    if let Some(s) = non_empty_string(ctx, by_name) {
        return Some(s);
    }
    let by_slot = ctx.get_field(service, slot);
    non_empty_string(ctx, by_slot)
}

fn provider_service_provider_obj(
    ctx: &mut dyn NativeContext,
    service: ObjectRef,
) -> Option<ObjectRef> {
    match ctx.get_field_by_name(service, "provider") {
        Value::Object(Some(p)) => Some(p),
        _ => match ctx.get_field(service, 2) {
            Value::Object(Some(p)) => Some(p),
            _ => None,
        },
    }
}

fn provider_service_class_name_from_registry(
    ctx: &mut dyn NativeContext,
    service: ObjectRef,
) -> Option<String> {
    let provider = provider_service_provider_obj(ctx, service)?;
    let provider_name = provider_name_of(ctx, provider);
    if provider_name.is_empty() {
        return None;
    }
    let type_str = provider_service_string_field(ctx, service, "type", 0)?;
    let algorithm = provider_service_string_field(ctx, service, "algorithm", 1)?;
    get_service_entry(&provider_name, &type_str, &algorithm).map(|entry| entry.class_name)
}

/// `Provider$Service.getClassName()` native — returns the entry's
/// implementation class name.  Required by the BC fallback path and by
/// `Cipher.getInstance(algo, providerName)` to render diagnostics when
/// resolution fails.  Reads slot 3 (populated in `make_service`).
/// `Provider.Service.getAttribute(String)` — answered from the attribute rows
/// the provider actually `put`, case-insensitively as the JDK's own `UString`
/// key is.
fn provider_service_get_attribute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(svc))) = args.first() else {
        return Ok(Some(Value::Object(None)));
    };
    let name = match args.get(1) {
        Some(Value::Object(Some(n))) => ctx.read_string(*n).unwrap_or_default(),
        // `getAttribute(null)` is a `NullPointerException` on the JDK.
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("attribute name is null".to_string()),
            }
            .into())
        }
    };
    let ih = ctx.identity_hash_code(*svc) as i64;
    let found = service_attributes_table().lock().get(&ih).and_then(|rows| {
        rows.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(&name))
            .map(|(_, v)| v.clone())
    });
    match found {
        Some(v) => {
            let s = ctx.create_string(&v);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn provider_service_get_class_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(class_name) = provider_service_string_field(ctx, this, "className", 3)
        .or_else(|| provider_service_class_name_from_registry(ctx, this))
    {
        let s = ctx.create_string(&class_name);
        return Ok(Some(Value::Object(Some(s))));
    }
    Ok(Some(Value::Object(None)))
}

/// `Provider$Service.toString()` must remain safe for the synthetic service
/// records returned by the provider chain.  The JDK implementation reaches
/// fields that synthetic records deliberately do not initialize.
fn provider_service_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let type_str = provider_service_string_field(ctx, this, "type", 0).unwrap_or_default();
    let algorithm = provider_service_string_field(ctx, this, "algorithm", 1).unwrap_or_default();
    let provider = provider_service_provider_obj(ctx, this)
        .map(|p| provider_name_of(ctx, p))
        .unwrap_or_else(|| "Provider".to_string());
    let class_name = provider_service_string_field(ctx, this, "className", 3)
        .or_else(|| provider_service_class_name_from_registry(ctx, this))
        .unwrap_or_default();
    let value = ctx.create_string(&format!(
        "{provider}: {type_str}.{algorithm} -> {class_name}"
    ));
    Ok(Some(Value::Object(Some(value))))
}

// ---------------------------------------------------------------------------
// Real-JCA bring-up — `sun.security.jca.GetInstance` bridge + reflective
// `Provider$Service.newInstance`.
//
// In real-JCA mode (CRATONVM_REAL_JCA) the synthetic KeyPairGenerator /
// KeyFactory / Signature short-circuits in `jca::key_factory` / `jca::signature`
// are NOT registered, so `KeyPairGenerator.getInstance(alg, "BC")` runs the
// real JDK 25 bytecode. That bytecode reaches
// `sun.security.jca.GetInstance.getService(type, algorithm, provider)`, which
// does `Providers.getProviderList().getProvider(provider)` — but our
// `Providers` shim returns null, so the JDK NPEs before it ever consults a
// `Provider`.
//
// These natives intercept `GetInstance.getService` (the documented entry point
// for `getInstance(type, clazz, algorithm, provider)`) and resolve the Service
// straight from OUR provider service map — the same map BouncyCastle populated
// via `Provider.put` / `parseLegacyPut`. The returned `Provider$Service` carries
// the real BC implementation class name; the real JDK bytecode then calls
// `service.newInstance(null)`, which our `provider_service_new_instance` native
// reflectively instantiates (real BC `*Spi`), so the *real* BC keygen/sign
// bytecode runs and yields concrete `BCECPrivateKey` / `BCRSAPublicKey`
// instances. No synthetic key material is fabricated here — these are pure
// JDK-bridge natives that route service resolution through our chain.
// ---------------------------------------------------------------------------

/// Build a `Provider$Service` for `(provider, type, algorithm)` from the global
/// service map, or `None` if the provider has no such entry. Materialises a
/// fresh `Provider` synthetic to attach as the service's owning provider.
fn resolve_service(
    ctx: &mut dyn NativeContext,
    provider: &str,
    type_str: &str,
    algo: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let entry = get_service_entry(provider, type_str, algo);
    if crate::nbflags().diag_jca {
        match &entry {
            Some(e) => eprintln!(
                "[JCA-DIAG] getService({provider},{type_str},{algo}) -> entry algo={:?} class={:?}",
                e.algorithm, e.class_name
            ),
            None => {
                let svc = services().lock();
                let count = svc.get(provider).map(|m| m.len()).unwrap_or(0);
                let kpg: Vec<String> = svc
                    .get(provider)
                    .map(|m| {
                        m.iter()
                            .filter(|((t, _), _)| t == &normalize_engine(type_str))
                            .map(|((_, a), e)| format!("{a}={}", e.class_name))
                            .collect()
                    })
                    .unwrap_or_default();
                eprintln!(
                    "[JCA-DIAG] getService({provider},{type_str},{algo}) -> NONE; provider has {count} entries; {type_str} entries: {kpg:?}"
                );
            }
        }
    }
    let Some(entry) = entry else {
        return Ok(None);
    };
    let (ver, coverage) = find(provider).unwrap_or((25.0, USER_PROVIDER_COVERAGE));
    let prov_obj = make_provider(ctx, provider, ver, coverage)?;
    Ok(Some(make_service(ctx, &entry, prov_obj)?))
}

/// `sun.security.jca.GetInstance.getService(String type, String algorithm,
/// String provider)` — provider-qualified resolution. Returns our
/// `Provider$Service` synthetic, or throws `NoSuchAlgorithmException`
/// (matching the JDK contract) when the provider has no matching entry.
fn getinstance_get_service_provider(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let type_str = read_arg_string(ctx, args, 0);
    let algo = read_arg_string(ctx, args, 1);
    let provider = read_arg_string(ctx, args, 2);
    match resolve_service(ctx, &provider, &type_str, &algo)? {
        Some(svc) => Ok(Some(Value::Object(Some(svc)))),
        // `RuntimeError::NotImplemented` is a VM-fatal error, not a Java
        // exception: it unwinds past every `catch` and ends the process. But
        // "this provider does not offer that algorithm" is an ORDINARY,
        // EXPECTED outcome of the method being intercepted here —
        // `GetInstance.getService` declares `NoSuchAlgorithmException` and
        // throws exactly this wording ("no such algorithm: X for provider Y",
        // measured on HotSpot 25). Callers routinely probe with it: bc-java's
        // `LEATest.testUnregisteredKeyGeneratorAliases` asks for a
        // deliberately-unregistered `KeyGenerator.LEAWRAP` and asserts the
        // refusal, and on this VM that assertion killed the whole
        // `jcajce.provider.test.AllTests` process instead of passing.
        None => Err(throw_no_such_algorithm(
            ctx,
            &format!("no such algorithm: {algo} for provider {provider}"),
        )),
    }
}

/// `sun.security.jca.GetInstance.getService(String type, String algorithm)` —
/// no-provider form: walk our provider chain in order and return the first
/// match (mirrors `ProviderList.getService`).
fn getinstance_get_service_search(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let type_str = read_arg_string(ctx, args, 0);
    let algo = read_arg_string(ctx, args, 1);
    for (name, _, _) in snapshot() {
        if let Ok(Some(svc)) = resolve_service(ctx, &name, &type_str, &algo) {
            return Ok(Some(Value::Object(Some(svc))));
        }
    }
    // Same reasoning as the provider-qualified form above — a Java-level
    // `NoSuchAlgorithmException`, not a VM-fatal `NotImplemented`. The
    // no-provider form's wording differs on HotSpot ("<algorithm> <type> not
    // available"), and callers do read it.
    Err(throw_no_such_algorithm(
        ctx,
        &format!("{algo} {type_str} not available"),
    ))
}

/// Resolve `(provider, type, algorithm)` to a real implementation class, build
/// its SPI via the real constructor, and wrap it in a `GetInstance$Instance`
/// (provider + impl) built via that class's real `(Provider, Object)`
/// constructor. This bypasses the `Provider$Service` object entirely — its
/// raw-slot className storage is not GC-stable (the synthetic object's
/// reference slots are not forwarded by the moving collector), whereas the
/// className here is read straight from the Rust-side `ServiceEntry` and the
/// resulting objects are constructed by their real `<init>` (proper, traced
/// fields). Returns `None` if no implementation is registered.
fn throw_no_such_algorithm(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/security/NoSuchAlgorithmException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    cratonvm_types::error::RuntimeError::SecurityException {
        message: msg.to_string(),
    }
    .into()
}

/// Mirrors real `sun.security.jca.GetInstance.getInstance(String, Class,
/// String, String)`'s own provider-existence check, which runs BEFORE any
/// algorithm lookup: an unregistered provider name is a `NoSuchProviderException`
/// ("no such provider: <name>"), never a `NoSuchAlgorithmException` — even when
/// no provider on the chain would have supplied the requested algorithm either.
/// Spring Boot's `JksSslStoreBundleTests.whenHasKeyStoreProvider` depends on this
/// ordering: it names a provider that was never registered at all, and asserts
/// the resulting `KeyStoreException`'s message (not a nested cause) contains the
/// provider name — which only the provider-existence message includes.
pub(crate) fn throw_no_such_provider(
    ctx: &mut dyn NativeContext,
    provider: &str,
    wording: ProviderArgWording,
) -> MethodCallFailed {
    let msg = match wording {
        ProviderArgWording::Shared => format!("no such provider: {provider}"),
        // `Cipher.getInstance` does its own lookup instead of going through
        // `GetInstance`, and capitalises both of its messages.
        ProviderArgWording::Cipher => format!("No such provider: {provider}"),
    };
    let detail = ctx.create_string(&msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/security/NoSuchProviderException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    cratonvm_types::error::RuntimeError::SecurityException { message: msg }.into()
}

/// Public wrapper over the module-private `throw_no_such_algorithm`, so engines
/// living in sibling modules raise a real `java.security.NoSuchAlgorithmException`
/// instead of reaching for whatever `RuntimeError` variant is nearest to hand.
pub(crate) fn throw_no_such_algorithm_public(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> MethodCallFailed {
    throw_no_such_algorithm(ctx, msg)
}

/// Which of real JDK's two message spellings a given engine uses. `Cipher`
/// rolls its own provider lookup and capitalises; everything routed through
/// `GetInstance` does not. Verified against HotSpot 25, not guessed.
#[derive(Clone, Copy)]
pub(crate) enum ProviderArgWording {
    Shared,
    Cipher,
}

/// Validate the `String provider` argument of a JCA
/// `getInstance(algorithm, provider)` overload, mirroring real JDK's ordering:
/// the provider is resolved BEFORE the algorithm is looked up, so
///
/// * a null/empty name is `IllegalArgumentException("missing provider")`, and
/// * an unregistered name is `NoSuchProviderException("no such provider: X")`
///
/// — never a `NoSuchAlgorithmException`, and never a silent success.
///
/// `KeyFactory`, `Signature`, `SecureRandom` and `Cipher` register ONE native
/// for all three `getInstance` overloads and read only argument 0, so the
/// provider argument was discarded entirely: asking a provider that was never
/// registered for an algorithm quietly succeeded. That is the same defect
/// `getinstance_instance_provider` fixes for every engine that does route
/// through `Security.getImpl` — these four just never reach it. Call this
/// first from any such native; it is a no-op for the single-argument and
/// `(algorithm, Provider)` overloads, which carry no name to check.
pub(crate) fn check_named_provider_arg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    idx: usize,
    wording: ProviderArgWording,
) -> Result<(), MethodCallFailed> {
    let Some(Value::Object(maybe_obj)) = args.get(idx) else {
        // Absent argument => the single-argument overload. Nothing to check.
        return Ok(());
    };
    let Some(obj) = maybe_obj else {
        // An explicit null. Real JDK folds null in with empty here.
        return Err(throw_missing_provider(ctx, wording));
    };
    // Discriminate the `(algorithm, String)` overload from `(algorithm,
    // Provider)` by the argument's actual class rather than by whether
    // `read_string` happens to succeed — a Provider object that read back as
    // an empty string would otherwise be reported as a missing provider.
    let is_string = ctx
        .class_name_of_id(ctx.class_id_of_object(*obj))
        .is_some_and(|n| n == "java/lang/String");
    if !is_string {
        return Ok(());
    }
    let name = ctx.read_string(*obj).unwrap_or_default();
    if name.is_empty() {
        return Err(throw_missing_provider(ctx, wording));
    }
    if find(&name).is_none() {
        return Err(throw_no_such_provider(ctx, &name, wording));
    }
    Ok(())
}

/// Enforce provider ownership for direct-native JCA factories after the
/// String-provider existence check. Provider-object overloads use the same
/// service table but need not be installed in `Security`.
pub(crate) fn check_provider_ownership(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    provider_idx: usize,
    engine: &str,
    algorithm: &str,
    wording: ProviderArgWording,
) -> Result<(), MethodCallFailed> {
    let Some(Value::Object(Some(provider_arg))) = args.get(provider_idx) else {
        return Ok(());
    };
    let is_string = ctx
        .class_name_of_id(ctx.class_id_of_object(*provider_arg))
        .is_some_and(|n| n == "java/lang/String");
    let provider = if is_string {
        ctx.read_string(*provider_arg).unwrap_or_default()
    } else {
        provider_name_of(ctx, *provider_arg)
    };
    if provider.is_empty() || provider == "<unknown>" {
        return Ok(());
    }
    let owned = get_service_entry(&provider, engine, algorithm).is_some()
        || (normalize_engine(engine) == "CIPHER"
            && algorithm
                .split('/')
                .next()
                .is_some_and(|base| get_service_entry(&provider, engine, base).is_some()));
    if owned {
        return Ok(());
    }
    let message = match wording {
        ProviderArgWording::Shared => {
            format!("no such algorithm: {algorithm} for provider {provider}")
        }
        ProviderArgWording::Cipher => format!("No such algorithm: {algorithm}"),
    };
    Err(throw_no_such_algorithm(ctx, &message))
}

fn throw_missing_provider(
    ctx: &mut dyn NativeContext,
    wording: ProviderArgWording,
) -> MethodCallFailed {
    let msg = match wording {
        ProviderArgWording::Shared => "missing provider",
        ProviderArgWording::Cipher => "Missing provider",
    };
    let _ = ctx;
    cratonvm_types::error::RuntimeError::IllegalArgumentException {
        message: msg.to_string(),
    }
    .into()
}

/// Return the name of the first provider in chain order whose service table
/// has a registered entry for `(type_str, algo)` — mirrors the search order
/// `getinstance_instance_search` uses, but only reports which provider owns
/// the algorithm rather than instantiating anything. For natives that build
/// their own JCA-engine object directly (bypassing `build_jca_instance`)
/// while still needing to attach the correct owning `Provider` — e.g.
/// `javax.net.ssl.KeyManagerFactory.getInstance(String)`'s own registration
/// in `phases_late.rs`, which used to hardcode the "SunJSSE" provider
/// unconditionally and so ignored any `KeyManagerFactory` service a caller
/// registered on their own `Provider` via `Security.addProvider` +
/// `Provider.put("KeyManagerFactory.<algo>", ...)`.
/// Every THIRD-PARTY provider's implementation class for `(type_str, algo)`, in
/// chain order.
///
/// [`third_party_service_class`] answers only the FIRST provider on the chain
/// and then discards it if that provider is one this VM services natively — so
/// a name that SunEC also registers hides every third-party implementation
/// behind it. That is fine for choosing a default and wrong for a fallback,
/// which needs the candidates the JDK's own delayed provider selection would
/// walk (`Signature$Delegate.chooseProvider` moves to the next provider when
/// the current one refuses the key).
pub(crate) fn chain_third_party_service_classes(type_str: &str, algo: &str) -> Vec<String> {
    snapshot()
        .into_iter()
        .filter(|(name, _, _)| {
            !NATIVELY_SERVICED_PROVIDERS
                .iter()
                .any(|b| b.eq_ignore_ascii_case(name))
        })
        .filter_map(|(name, _, _)| get_service_entry(&name, type_str, algo))
        .map(|e| e.class_name.replace('.', "/"))
        .filter(|c| !c.trim().is_empty())
        .collect()
}

/// A third-party provider that owns one of `algos` and sits AHEAD of `before`
/// in the installed chain.
///
/// `getInstance(algorithm)` with no provider named is defined by chain ORDER:
/// the first installed provider that has the service wins. This engine answers
/// as one particular provider (`SunJCE` for `Cipher`), so serving a name it can
/// compute is right only while nothing ahead of that provider owns the name
/// too. An application that calls `Security.insertProviderAt(p, 2)` has said
/// exactly that it wants `p` consulted first, and bc-java's `SlotTwoTest` does
/// it and then asserts `decrypt.getProvider().getName()` is `BC` — it got
/// `SunJCE`, for `DESede/ECB/PKCS7Padding`, a padding spelling SunJCE does not
/// even register.
///
/// Deliberately narrow: providers at or after `before` are not consulted, so a
/// third-party provider left at its default position (the end of the chain,
/// where `Security.addProvider` puts it) changes nothing. Only an explicit
/// insertion ahead of this engine's own identity does.
pub(crate) fn third_party_owner_before(
    type_str: &str,
    algos: &[String],
    before: &str,
) -> Option<String> {
    let names: Vec<String> = snapshot().into_iter().map(|(name, _, _)| name).collect();
    let limit = names.iter().position(|n| n.eq_ignore_ascii_case(before))?;
    names.into_iter().take(limit).find(|name| {
        !NATIVELY_SERVICED_PROVIDERS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(name))
            && algos
                .iter()
                .any(|algo| get_service_entry(name, type_str, algo).is_some())
    })
}

pub(crate) fn find_service_provider(type_str: &str, algo: &str) -> Option<String> {
    snapshot()
        .into_iter()
        .find(|(name, _, _)| get_service_entry(name, type_str, algo).is_some())
        .map(|(name, _, _)| name)
}

/// The JDK providers this VM services from Rust rather than from the class
/// named in their service table.
///
/// The seeded entries for these carry the REAL JDK's implementation class
/// names, which are the names the native engines stand in for — instantiating
/// them instead would bypass every native implementation in this crate. Only
/// providers OUTSIDE this set are application code that must actually be run.
const NATIVELY_SERVICED_PROVIDERS: &[&str] = &[
    "SUN",
    "SunRsaSign",
    "SunEC",
    "SunJSSE",
    "SunJCE",
    "SunJGSS",
    "SunSASL",
    "XMLDSig",
    "SunPCSC",
    "JdkLDAP",
    "JdkSASL",
    "SunMSCAPI",
    "SunPKCS11",
];

/// The implementation class a THIRD-PARTY provider registered for
/// `(type_str, algo)`, or `None` when no such provider offers it.
///
/// `Security.addProvider(myProvider)` + `put("Signature.SHA256withRSA",
/// MySpi.class.getName())` is the documented way to supply an implementation
/// the platform does not have — a smartcard, an HSM, a delegating key. This VM
/// recorded those registrations (they show up in `getServices()` and in
/// `getInstance`'s "is it offered" gate) but never instantiated the class, so
/// every such `Signature` was silently serviced by the built-in native engine
/// instead. For a key only the application's provider can use — precisely the
/// case a third-party provider exists for — that engine cannot work, and
/// netty's `JdkDelegatingPrivateKeyMethod` (`MockAlternativeKeyProvider` in
/// `JdkDelegatingPrivateKeyMethodTest`) is exactly that shape.
///
/// `provider` `None` = "no provider requested", which searches the chain in
/// order, the same order `Signature.getInstance(String)` walks.
pub(crate) fn third_party_service_class(
    provider: Option<&str>,
    type_str: &str,
    algo: &str,
) -> Option<String> {
    let is_third_party = |name: &str| {
        !NATIVELY_SERVICED_PROVIDERS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(name))
    };
    let name = match provider {
        Some(p) => {
            if !is_third_party(p) {
                return None;
            }
            p.to_string()
        }
        None => snapshot()
            .into_iter()
            .find(|(name, _, _)| get_service_entry(name, type_str, algo).is_some())
            .map(|(name, _, _)| name)
            .filter(|name| is_third_party(name))?,
    };
    let entry = get_service_entry(&name, type_str, algo)?;
    if entry.class_name.trim().is_empty() {
        return None;
    }
    Some(entry.class_name.replace('.', "/"))
}

/// The implementation class a JDK provider registered for `(type_str, algo)` —
/// the twin of [`third_party_service_class`] for the providers this crate
/// normally services natively.
///
/// # This is legitimate ONLY on an engine's refusal path
///
/// [`NATIVELY_SERVICED_PROVIDERS`] exists because a chain walk that reached the
/// JDK's own class FIRST would bypass every native implementation in this
/// crate — a `Cipher.getInstance("AES")` served by `com.sun.crypto.provider
/// .AESCipher` instead of the Rust engine is not the VM anybody is testing.
/// That argument is about ORDER, not about the class being unusable, and
/// `jca::cipher::try_delegate_cipher_to_chain` already writes the discipline
/// down: "deliberately ordered AFTER this engine's own verdict, never before
/// it". Called there, the JDK class is reached only for names the native
/// engine has just refused, so no existing answer changes and the refusal is
/// replaced by the platform's own implementation.
///
/// # Why this is worth having at all
///
/// Measured on 2026-09-02, all 84 of the implementation classes behind
/// `jca-provider-population-gap-20260830.md`'s functional gap LOAD on this VM
/// (`apps/probes/JcaGapSizer --check`: `loads=yes` for every one; the
/// `instantiates=` column reports the probe's own
/// `InaccessibleObjectException` from `setAccessible` on a non-exported
/// package, which is not how the JCA constructs them). So for a large part of
/// that gap the implementation is already present and only the route to it was
/// missing.
///
/// Returns `None` for the placeholder class names this crate seeds for its own
/// natively-served rows (`sun.security.provider.Native`,
/// `com.sun.crypto.provider.Native`). Those are not classes; they are markers
/// saying "a Rust engine answers this", and instantiating them would raise
/// `ClassNotFoundException` on a path whose whole job is to be a quiet
/// fallback.
pub(crate) fn jdk_service_class(
    provider: Option<&str>,
    type_str: &str,
    algo: &str,
) -> Option<(String, String)> {
    let is_jdk = |name: &str| {
        NATIVELY_SERVICED_PROVIDERS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(name))
    };
    let name = match provider {
        Some(p) => {
            if !is_jdk(p) {
                return None;
            }
            p.to_string()
        }
        None => snapshot()
            .into_iter()
            .find(|(name, _, _)| {
                is_jdk(name) && get_service_entry(name, type_str, algo).is_some()
            })
            .map(|(name, _, _)| name)?,
    };
    let entry = get_service_entry(&name, type_str, algo)?;
    let class_name = entry.class_name.trim();
    if class_name.is_empty() || class_name.ends_with(".Native") {
        return None;
    }
    Some((name, class_name.to_string()))
}

/// The REAL implementation class a provider registered for `(type, algo)`, or
/// `None` when the row is absent, empty, or carries the `.Native` marker.
///
/// The marker is not a class. It means "a Rust engine in this crate answers
/// this", so a caller asking "is there something to instantiate here?" must get
/// `None` for it — otherwise every natively-served row looks like a delegable
/// one and `build_jca_impl` is sent after a class that does not exist.
pub(crate) fn service_implementation_class(
    type_str: &str,
    provider: &str,
    algo: &str,
) -> Option<String> {
    let entry = get_service_entry(provider, type_str, algo)?;
    let class_name = entry.class_name.trim();
    if class_name.is_empty() || class_name.ends_with(".Native") {
        return None;
    }
    Some(class_name.to_string())
}

/// Every `SSLContext` protocol name SunJSSE registers on JDK 25, ASCII-
/// uppercased.
///
/// Measured, not guessed: enumerating
/// `Security.getProvider("SunJSSE").getServices()` on jdk-25.0.3.9-hotspot
/// yields primaries {TLS, TLSv1, TLSv1.1, TLSv1.2, TLSv1.3, Default, DTLS,
/// DTLSv1.0, DTLSv1.2} plus aliases {SSL -> TLS, SSLv3 -> TLSv1}. Anything
/// outside that set raises `NoSuchAlgorithmException` on HotSpot — confirmed
/// for `SSLv2`, `TLSv1.4` and `NO-SUCH-TLS`.
const SSL_CONTEXT_PROTOCOLS: &[&str] = &[
    "TLS", "TLSV1", "TLSV1.1", "TLSV1.2", "TLSV1.3", "SSL", "SSLV3", "DEFAULT", "DTLS", "DTLSV1.0",
    "DTLSV1.2",
];

/// Can `SSLContext.getInstance(protocol)` be serviced?
///
/// W3-7 (`RJdkSecurity.tls`): the two real-JDK-mode `SSLContext.getInstance`
/// registrations each carried their OWN hand-rolled protocol list, and both
/// were narrower than the platform's — `net_phase_e::register_re6_ssl_context`
/// accepted five names, `phases_late::ssl_security::register_p68_ssl` seven,
/// and neither knew about DTLS. A too-narrow list is the dangerous direction
/// here: it turns a valid `getInstance("TLSv1")` into a refusal and takes
/// every HTTPS-using suite down with it. So this answers YES on two grounds
/// and NO only when both fail:
///
///   1. the name is in the measured JDK-25 SunJSSE set above; or
///   2. some provider in the live chain actually registered an `SSLContext`
///      service under that name — a caller-installed provider (Conscrypt,
///      BC-JSSE, Elytron) legitimately adds protocols we have never heard of,
///      and refusing those would be the same defect one layer up.
///
/// It is NOT the place to decide whether a protocol is *safe*; `SSLv3` and
/// `TLSv1` resolve here exactly as they do on HotSpot, and the enabled-
/// protocol policy that actually keeps them off the wire lives in the
/// connector (`new13_build_connector` pins a TLS 1.2 floor).
pub(crate) fn ssl_context_protocol_supported(protocol: &str) -> bool {
    // Do NOT copy `message_digest::algorithm_supported`'s alphanumeric-strip
    // normalise here. Measured on jdk-25.0.3.9-hotspot: `TLSV1.2`, `tlsv1.3`,
    // `SSLV3` and `dtls` all resolve, while `TLSv12`, `TLS `, ` TLS` and
    // `T-L-S` all raise `NoSuchAlgorithmException`. JCA lookup is case-
    // insensitive and nothing else. Digest names get the cruder normalise
    // because the JDK's own tables carry `SHA256`/`SHA-256` aliases; the
    // SSLContext table carries none, so stripping punctuation would fabricate
    // `getInstance("TLSv12")` into a working context — the exact defect
    // species this predicate exists to close.
    if protocol != protocol.trim() {
        return false;
    }
    let upper = protocol.to_ascii_uppercase();
    if SSL_CONTEXT_PROTOCOLS.contains(&upper.as_str()) {
        return true;
    }
    find_service_provider("SSLContext", protocol).is_some()
}

/// `Security.getAlgorithms(serviceName)` — the algorithm-name set for one
/// engine type across EVERY provider in the live chain, answered from the
/// same `services()` registry that backs `Provider.getService` /
/// `Provider.getServices` / `find_service_provider`.
///
/// ## W4-3 — why this exists at all
///
/// In real-JDK mode there was no `Security.getAlgorithms` native, so the call
/// ran real JDK 25 bytecode, which iterates `provider.keys()` over the
/// `Provider` objects `Security.getProviders()` handed back. Those are the
/// synthetics `make_provider` builds — their inherited `Hashtable` is empty by
/// construction, and `make_provider`'s own doc comment already says so:
/// "`keys()` returns an empty enumeration and `getAlgorithms` yields an empty
/// set rather than throwing". Empty. For every engine type. That is
/// `RJdkSecurity.providers` (:311) failing `MessageDigest algorithms must
/// include SHA-256` in both `--real-jdk` and `--jdk-only`.
///
/// The only other implementation lived in
/// `phases_early::register_phase53_security`, which is reached solely from
/// `register_synthetic_overrides` — i.e. `--synthetic-jdk` builds. It carried a
/// hand-maintained six-entry-per-type literal table. It now delegates here, so
/// the two modes cannot disagree and the answer cannot drift from the registry
/// that decides `getService`.
///
/// ## Semantics — measured on jdk-25.0.3.9-hotspot, not recalled
///
/// A probe enumerating `Security.getAlgorithms(t)` for 28 engine types
/// established every rule below:
///   * names come back ASCII-UPPERCASED (`SHA-256`, `HMACSHA256`,
///     `AES/GCM/NOPADDING`) and `contains` is case-SENSITIVE:
///     `contains("SHA-256")` is true, `contains("sha-256")` is false;
///   * ALIASES ARE EXCLUDED. `MessageDigest` answers 15 names, exactly the 15
///     primary `MessageDigest.*` services SUN registers — the `SHA256` alias
///     of `SHA-256` is absent, because its property key is
///     `Alg.Alias.MessageDigest.SHA256`, which does not start with
///     `MESSAGEDIGEST`. So this walks `services()` and never `aliases()`;
///   * attribute keys (`MessageDigest.SHA-256 ImplementedIn`) are skipped
///     because they contain a space — kept here for faithfulness even though
///     `put_service` never stores an attribute as its own entry;
///   * the match is `startsWith` on the whole `TYPE.ALGORITHM` key and the cut
///     is `serviceName.length() + 1` characters, NOT an exact type equality.
///     That is a real JDK quirk (`getAlgorithms("Key")` yields `ACTORY.RSA`);
///     reproduced rather than "fixed" so the two agree;
///   * a null, empty, or `.`-terminated service name yields the EMPTY set.
///
/// (A "Deliberate divergence, recorded" paragraph stood here saying the caller
/// gets a plain `HashSet` where HotSpot returns `Collections.unmodifiableSet`.
/// That divergence is closed — `security_get_algorithms` and
/// `provider_get_services_native` both wrap through `wrap_unmodifiable` now.
/// A comment outlives its defect. W7-63-jca-advertise-vs-serve.md.)
/// `true` when SOME registered provider offers `(type_str, algo)`.
///
/// The thin question behind `find_service_provider`, for callers that only need
/// to know whether anything can serve a name — `KeyPairGenerator.getInstance`
/// asks it to decide between handing back a generator and raising
/// `NoSuchAlgorithmException`.
///
/// Note what this does NOT cover: the algorithms CratonVM serves from its own
/// natives are not in the service registry at all, so a `false` here is only
/// half the answer. `kpg_serviceable` is the other half.
pub(crate) fn any_provider_offers(type_str: &str, algo: &str) -> bool {
    find_service_provider(type_str, algo).is_some()
}

/// `true` when `provider` specifically offers `(type_str, algo)` — the
/// two-argument `getInstance(alg, provider)` question, which must NOT be
/// satisfied by some other provider that happens to have the algorithm.
pub(crate) fn provider_offers(provider: &str, type_str: &str, algo: &str) -> bool {
    get_service_entry(provider, type_str, algo).is_some()
}

pub(crate) fn algorithms_for_service(service_name: &str) -> Vec<String> {
    if service_name.is_empty() || service_name.ends_with('.') {
        return Vec::new();
    }
    let prefix = service_name.to_ascii_uppercase();
    let cut = service_name.len() + 1;
    // Take the chain snapshot BEFORE locking `services()`: `snapshot()` locks
    // `provider_chain()`, and no path in this module takes those two in the
    // other order. Provider order is preserved so the result is deterministic.
    let chain = snapshot();
    let table = services().lock();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for (provider, _, _) in chain {
        let Some(map) = table.get(&provider) else {
            continue;
        };
        for entry in map.values() {
            let key = format!("{}.{}", entry.type_str, entry.algorithm).to_ascii_uppercase();
            if !key.starts_with(prefix.as_str()) || key.contains(' ') {
                continue;
            }
            // Byte index: `cut` counts chars of the caller's service name.
            // `str::get` returns `None` on a non-boundary index rather than
            // panicking, so a multi-byte name degrades to "no match".
            let Some(name) = key.get(cut..) else {
                continue;
            };
            if !name.is_empty() && seen.insert(name.to_string()) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Wrap `set` in `Collections.unmodifiableSet`, which is what HotSpot returns
/// from BOTH `Security.getAlgorithms` and `Provider.getServices` — measured on
/// jdk-25.0.3.9-hotspot and recorded in
/// probes/JcaAdvertisedVsServedProbe.expected.txt §D:
/// `java.util.Collections$UnmodifiableSet`, with `add`, `remove` and
/// `iterator().remove()` all raising `UnsupportedOperationException`.
///
/// This is not a missing guard, it is a missing STATEMENT.
/// `Collections.unmodifiableSet` is how the JDK tells a caller "this is a view
/// of platform state, not yours to edit". A plain `HashSet` is an invitation:
/// a caller that mutates it and hands it on has manufactured an algorithm list
/// no provider backs, and nothing downstream can tell it from a real one.
///
/// Applied on EVERY path including the empty ones — `getAlgorithms` for an
/// unknown engine type answers an IMMUTABLE EMPTY set on HotSpot, never a
/// throw. HotSpot distinguishes `Collections$UnmodifiableSet` (unknown engine
/// type) from `Collections$EmptySet` (`""`, `"Foo."`, `null`); that split is
/// not reproduced, because both are immutable and both are size 0, which is
/// the entire observable contract.
///
/// Each HotSpot call returns a DISTINCT object (measured: two calls are not
/// `==`), so this must be built per call and must not be cached.
///
/// GC: `unmodifiableSet` allocates the view, so `set` must be a freshly
/// re-read reference and only the RETURN value may be used afterwards.
///
/// On failure the plain set is returned rather than propagating the error: an
/// immutability wrapper is not worth converting a correct answer into a thrown
/// exception. The `Ok(None)` / `Ok(Some(null))` fallback should be unreachable.
///
/// **Mode caveat, and it is a vacuous-green trap for whoever verifies this.**
/// `java.util.Collections.unmodifiableSet` has THREE registrations in this
/// tree and registration is last-write-wins:
///
///   * `native-collections`'s `register_collections_extras_natives` binds it to
///     `native_collections_unmodifiable_set`, which allocates a genuine
///     read-only view. Live in real-JDK and `--jdk-only`, where it is what
///     this call reaches (in real-JDK mode real JDK bytecode is available
///     too — either way the result is immutable).
///   * `phases_early::register_collections_extras_natives` AND
///     `phases_early::register_core_stdlib_extras` both bind it to
///     `native_return_first_arg` — the IDENTITY function. Both are reached
///     only from `lib::register_synthetic_overrides`, which runs after the
///     essential registrars, so **in `--synthetic-jdk` the identity wins and
///     this wrapper is inert**: the caller gets the same mutable `HashSet`
///     back and `add` still succeeds.
///
/// So a probe run under `--synthetic-jdk` will report
/// `class=java.util.HashSet add=SUCCEEDED` here and that is NOT evidence this
/// change failed to land — it is a separate defect one layer down, recorded in
/// W7-63-jca-advertise-vs-serve.md §8. An "unmodifiable" wrapper that returns
/// its argument is the same species as everything else in that record: an API
/// whose whole contract is a refusal, quietly not refusing.
fn wrap_unmodifiable(ctx: &mut dyn NativeContext, set: ObjectRef) -> ObjectRef {
    match ctx.invoke(
        "java/util/Collections",
        "unmodifiableSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        &[Value::Object(Some(set))],
    ) {
        Ok(Some(Value::Object(Some(view)))) => view,
        _ => set,
    }
}

/// `wrap_unmodifiable` for the `--synthetic-jdk` `Security.getAlgorithms`
/// override in `phases_early`, which deliberately shadows the registry-backed
/// registration in that mode and therefore has to make the same answer in the
/// same shape.
pub(crate) fn wrap_unmodifiable_public(ctx: &mut dyn NativeContext, set: ObjectRef) -> ObjectRef {
    wrap_unmodifiable(ctx, set)
}

fn security_get_algorithms(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `Security.getAlgorithms(null)` returns the EMPTY set on HotSpot (the
    // real body's first branch), it does not NPE — measured.
    let service_name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let names = algorithms_for_service(&service_name);
    // GC-safety: `ctx.create_string` allocates, so a `Vec<Value>` of
    // already-created strings built in one pass would hold pre-move
    // addresses. Same shape as `provider_get_services_native` above: build the
    // set first, pin it, and add one pinned element at a time.
    let set = cratonvm_native_collections::make_hashset_with_elements(ctx, &[])?;
    let set_pin = ctx.pin_native_root(set);
    for name in names {
        let s = ctx.create_string(&name);
        let s_pin = ctx.pin_native_root(s);
        let set = ctx.read_native_pin(set_pin, set);
        let s = ctx.read_native_pin(s_pin, s);
        let add_result = ctx.invoke(
            "java/util/HashSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(set)), Value::Object(Some(s))],
        );
        ctx.unpin_native_roots(s_pin);
        if let Err(e) = add_result {
            ctx.unpin_native_roots(set_pin);
            return Err(e);
        }
    }
    let set = ctx.read_native_pin(set_pin, set);
    // Wrap BEFORE unpinning: `wrap_unmodifiable` invokes Java, which can move
    // `set`, and the pin is what keeps the just-re-read reference valid across
    // that call. Only the returned view may be used afterwards.
    let view = wrap_unmodifiable(ctx, set);
    ctx.unpin_native_roots(set_pin);
    Ok(Some(Value::Object(Some(view))))
}

/// Resolve `name` to the best available `Provider` object: the REAL
/// user-registered instance if one is on file (see `real_provider_table` /
/// `remember_real_provider`), so `getInfo()`/`getName()` return exactly what
/// the caller's own `Provider` constructor set — otherwise a fresh synthetic
/// built from the seed-list entry (or a generic `USER_PROVIDER_COVERAGE`
/// synthetic if `name` isn't in the seed list at all).
pub(crate) fn resolve_or_make_provider(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(real) = resolve_real_provider(ctx, name) {
        return Ok(real);
    }
    let (ver, coverage) = find(name).unwrap_or((1.0, USER_PROVIDER_COVERAGE));
    Ok(make_provider(ctx, name, ver, coverage)?)
}

pub(crate) fn build_jca_impl(
    ctx: &mut dyn NativeContext,
    provider: &str,
    type_str: &str,
    algo: &str,
) -> Option<MethodCallResult> {
    let entry = get_service_entry(provider, type_str, algo)?;
    if entry.class_name.is_empty() {
        return None;
    }
    // Try the provider's own `EngineCreator` factory first when the real
    // provider object is on file (see `try_engine_creator_instantiate`) —
    // required for BC-FIPS, whose `className` is a non-loadable label.
    let engine_creator_result = resolve_real_provider(ctx, provider).and_then(|provider_obj| {
        try_engine_creator_instantiate(ctx, provider_obj, &entry.class_name)
    });
    Some((|| {
        // Instantiate the real SPI/engine object (runs genuine provider bytecode).
        let impl_ref = if let Some(result) = engine_creator_result {
            match result? {
                Some(Value::Object(Some(o))) => o,
                _ => {
                    return Err(
                        cratonvm_types::error::RuntimeError::ClassNotFoundException {
                            class_name: entry.class_name.clone(),
                        }
                        .into(),
                    )
                }
            }
        } else {
            // Class-loading here can fail with a non-catchable internal
            // `VmError` (`new_object_initialized` resolves classes through
            // the low-level `load_class_concurrent` bootstrap path, which is
            // NOT the same as `Class.forName` — a miss there is meant to be
            // fatal for genuine VM-internal lookups). `className` is
            // arbitrary data supplied by whatever provider registered this
            // service, so a lookup miss here is an ordinary, expected
            // outcome and must surface as a catchable Java exception
            // instead of aborting the process.
            let internal = entry.class_name.replace('.', "/");
            match ctx.new_object_initialized(&internal, "()V", &[]) {
                Ok(Some(Value::Object(Some(o)))) => o,
                Err(MethodCallFailed::ExceptionThrown(t)) => {
                    return Err(MethodCallFailed::ExceptionThrown(t))
                }
                _ => {
                    return Err(
                        cratonvm_types::error::RuntimeError::ClassNotFoundException {
                            class_name: entry.class_name.clone(),
                        }
                        .into(),
                    )
                }
            }
        };
        Ok(Some(Value::Object(Some(impl_ref))))
    })())
}

fn build_jca_instance(
    ctx: &mut dyn NativeContext,
    provider: &str,
    type_str: &str,
    algo: &str,
) -> Result<Option<MethodCallResult>, MethodCallFailed> {
    let Some(impl_result) = build_jca_impl(ctx, provider, type_str, algo) else {
        return Ok(None);
    };
    Ok(Some((|| {
        let impl_ref = match impl_result? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NotImplemented {
                    feature: format!(
                    "{type_str} {algo} implementation for provider {provider} returned no object"
                ),
                }
                .into())
            }
        };
        // Pin the SPI across the Provider allocation below (which can GC).
        let pin = ctx.pin_native_root(impl_ref);
        let (ver, coverage) = find(provider).unwrap_or((25.0, USER_PROVIDER_COVERAGE));
        let prov_obj = make_provider(ctx, provider, ver, coverage)?;
        let impl_ref = ctx.read_native_pin(pin, impl_ref);
        // 2. Build GetInstance$Instance(provider, impl) via its real ctor.
        let inst = ctx.new_object_initialized(
            "sun/security/jca/GetInstance$Instance",
            "(Ljava/security/Provider;Ljava/lang/Object;)V",
            &[Value::Object(Some(prov_obj)), Value::Object(Some(impl_ref))],
        );
        ctx.unpin_native_roots(pin);
        inst
    })()))
}

fn getinstance_instance_provider(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (String type, Class clazz, String algorithm, String provider)
    let type_str = read_arg_string(ctx, args, 0);
    let algo = read_arg_string(ctx, args, 2);
    let provider = read_arg_string(ctx, args, 3);
    if find(&provider).is_none() {
        return Err(throw_no_such_provider(
            ctx,
            &provider,
            ProviderArgWording::Shared,
        ));
    }
    match build_jca_instance(ctx, &provider, &type_str, &algo)? {
        Some(r) => r,
        None => Err(throw_no_such_algorithm(
            ctx,
            // Real `GetInstance.getInstance` reports
            // "no such algorithm: <algo> for provider <p>"; the engine type is
            // not part of the message. Verified against HotSpot 25.
            &format!("no such algorithm: {algo} for provider {provider}"),
        )),
    }
}

/// `Provider.getService(String, String)`, as the JDK declares it.
const PROVIDER_GET_SERVICE_DESC: &str =
    "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;";

/// Resolve `(type, algorithm)` through a Provider OBJECT that declares its own
/// `getService`, exactly as `sun.security.jca.GetInstance` does: call the
/// override, then `newInstance(null)` on whatever `Provider$Service` it hands
/// back — both virtually, so a `Provider$Service` SUBCLASS runs its own
/// instantiation logic. Returns `Ok(None)` when this provider does not override
/// `getService` (our own synthetics never do) or when the override answers
/// null, leaving the caller on its side-table path.
///
/// Why this exists: a provider that overrides `getService` may register a
/// legacy `put` value that is a MARKER, not a loadable class name, and keep the
/// real factory somewhere only its own `Service` subclass can see. BouncyCastle's
/// JSSE provider is exactly that shape — `addAlgorithmImplementation` puts
/// `"org.bouncycastle.jsse.provider.SSLContext.TLSv1_3"` under the legacy key
/// `SSLContext.TLSv1.3` and stashes the real factory in a private `creatorMap`
/// that only `BouncyCastleJsseProvider$BcJsseService.newInstance` consults.
/// Resolving it out of OUR service map instead reached `Class.forName` on that
/// marker, so `SSLContext.getInstance("TLSv1.3", new BouncyCastleJsseProvider())`
/// died with `ClassNotFoundException: org.bouncycastle.jsse.provider
/// .SSLContext.TLSv1_3` where HotSpot ran BC's creator (netty's
/// `BouncyCastleEngineAlpnTest`).
///
/// The side-table path is still the default for everything else: it hands back a
/// `GetInstance$Instance` built from the Rust-side `ServiceEntry` without a
/// `Provider$Service` round-trip, whose synthetic's className slot is not
/// GC-stable (see `build_jca_instance`).
fn provider_declared_service_instance(
    ctx: &mut dyn NativeContext,
    provider: ObjectRef,
    type_str: &str,
    algo: &str,
) -> Result<Option<MethodCallResult>, MethodCallFailed> {
    let cid = ctx.class_id_of_object(provider);
    if !ctx.class_declares_method(cid, "getService", PROVIDER_GET_SERVICE_DESC) {
        return Ok(None);
    }
    // Every `create_string` below can move the receiver, so pin first and
    // re-read through the pin after each allocation.
    let prov_pin = ctx.pin_native_root(provider);
    let type_s0 = ctx.create_string(type_str);
    let type_pin = ctx.pin_native_root(type_s0);
    let algo_s0 = ctx.create_string(algo);
    let algo_pin = ctx.pin_native_root(algo_s0);
    let provider = ctx.read_native_pin(prov_pin, provider);
    let type_s = ctx.read_native_pin(type_pin, type_s0);
    let algo_s = ctx.read_native_pin(algo_pin, algo_s0);
    let svc = match ctx.invoke_virtual(
        provider,
        "getService",
        PROVIDER_GET_SERVICE_DESC,
        &[Value::Object(Some(type_s)), Value::Object(Some(algo_s))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => s,
        // Null is the JDK's "this provider does not offer that algorithm"
        // answer; the caller turns it into NoSuchAlgorithmException itself.
        Ok(_) => {
            ctx.unpin_native_roots(prov_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(prov_pin);
            return Err(e);
        }
    };
    let svc_pin = ctx.pin_native_root(svc);
    let svc = ctx.read_native_pin(svc_pin, svc);
    // A throw here (BC's `ProvSSLContextSpi.<clinit>` raising on a mismatched
    // bcprov, for one) is the provider's own failure and must reach the caller
    // unchanged — that IS the HotSpot behaviour.
    let impl_ref = match ctx.invoke_virtual(
        svc,
        "newInstance",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(None)],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(prov_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(prov_pin);
            return Err(e);
        }
    };
    let impl_pin = ctx.pin_native_root(impl_ref);
    let provider = ctx.read_native_pin(prov_pin, provider);
    let impl_ref = ctx.read_native_pin(impl_pin, impl_ref);
    let inst = ctx.new_object_initialized(
        "sun/security/jca/GetInstance$Instance",
        "(Ljava/security/Provider;Ljava/lang/Object;)V",
        &[Value::Object(Some(provider)), Value::Object(Some(impl_ref))],
    );
    ctx.unpin_native_roots(prov_pin);
    Ok(Some(inst))
}

fn getinstance_instance_provider_obj(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (String type, Class clazz, String algorithm, Provider provider)
    let type_str = read_arg_string(ctx, args, 0);
    let algo = read_arg_string(ctx, args, 2);
    // Read the name BEFORE anything allocates: `provider_declared_service_instance`
    // pins its own receiver, but `p` here would go stale across it otherwise.
    let prov_ref = match args.get(3) {
        Some(Value::Object(Some(p))) => Some(*p),
        _ => None,
    };
    let provider = match prov_ref {
        Some(p) => read_provider_name_version(ctx, p)
            .map(|(n, _)| n)
            .unwrap_or_default(),
        None => String::new(),
    };
    if let Some(p) = prov_ref {
        if let Some(r) = provider_declared_service_instance(ctx, p, &type_str, &algo)? {
            return r;
        }
    }
    match build_jca_instance(ctx, &provider, &type_str, &algo)? {
        Some(r) => r,
        None => Err(throw_no_such_algorithm(
            ctx,
            &format!("no {type_str} {algo} implementation for provider {provider}"),
        )),
    }
}

/// `sun.security.jca.GetInstance.getServices(String type, String algorithm)` —
/// returns an `Iterator<Provider$Service>` over every matching service in the
/// provider chain. Used by the lazy `KeyFactory(String)` / `Signature(String)`
/// / `Cipher` constructors (`serviceIterator` pattern) whose path does NOT go
/// through `GetInstance.getInstance`; without this the bytecode reaches
/// `Providers.getProviderList()` (which our shim leaves null) and NPEs on
/// `list.getServices(...)`.
fn getinstance_get_services(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let type_str = read_arg_string(ctx, args, 0);
    let algo = read_arg_string(ctx, args, 1);
    let al = "java/util/ArrayList";
    let al_cid = ctx.ensure_class_initialized(al).map_err(|_| {
        cratonvm_types::error::RuntimeError::NotImplemented {
            feature: "GetInstance.getServices: ArrayList not loaded".into(),
        }
    })?;
    let mut list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    ctx.invoke(al, "<init>", "()V", &[Value::Object(Some(list))])?;
    let pin = ctx.pin_native_root(list);
    for (name, _, _) in snapshot() {
        if let Ok(Some(svc)) = resolve_service(ctx, &name, &type_str, &algo) {
            list = ctx.read_native_pin(pin, list);
            ctx.invoke(
                al,
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(list)), Value::Object(Some(svc))],
            )?;
        }
    }
    list = ctx.read_native_pin(pin, list);
    let it = ctx.invoke(
        al,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(list))],
    );
    ctx.unpin_native_roots(pin);
    it
}

fn getinstance_instance_search(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (String type, Class clazz, String algorithm) — search the chain.
    let type_str = read_arg_string(ctx, args, 0);
    let algo = read_arg_string(ctx, args, 2);
    for (name, _, _) in snapshot() {
        if let Ok(Some(r)) = build_jca_instance(ctx, &name, &type_str, &algo) {
            return r;
        }
    }
    // Every real `getInstance(type, algorithm)` overload declares (and
    // catches, for callers like `KeyStore.getInstance(String)` which wraps it
    // into `KeyStoreException`) `NoSuchAlgorithmException` for exactly this
    // case. Previously this was a Rust-level `RuntimeError::NotImplemented`,
    // which surfaces as `MethodCallFailed::InternalError` -- NOT catchable by
    // Java `try`/`catch` -- and is only caught at `main-vm`'s top-level `Err`
    // handler, which prints `"[cratonvm] main-vm run() returned Err: ..."`
    // and exits the whole process. That turned a per-algorithm
    // NoSuchAlgorithmException (e.g. `AlgorithmParameters.getInstance("OAEP")`
    // before it was registered, or `KeyStore.getInstance("BCFKS")`, which
    // WildFly Elytron deliberately does NOT support and probes for via a
    // `catch (KeyStoreException e)` in `CryptoProvider.getSupportedKeyStoreTypes()`)
    // into a crash of the entire test process instead of one clean, catchable
    // exception.
    Err(throw_no_such_algorithm(
        ctx,
        // Real `GetInstance.getInstance(type, clazz, algorithm)` reports
        // "<algo> <type> not available" when no provider supplies it.
        // Verified against HotSpot 25.
        &format!("{algo} {type_str} not available"),
    ))
}

/// Real-JCA bring-up: build a genuine `java.security.cert.CertificateFactory`
/// wrapping a real provider's `CertificateFactorySpi`, for
/// `native-builtins/src/phases_late.rs`'s `CertificateFactory.getInstance
/// (String)` native — the ONLY natively-intercepted overload of `getInstance`
/// on this class (`getInstance(String, Provider)`/`getInstance(String,
/// String)` are never intercepted at all and already reach real bytecode,
/// which resolves through this same provider map via
/// `getinstance_instance_provider`/`getinstance_instance_provider_obj`).
/// Without this, `getInstance(String)` always handed out a 1-field synthetic
/// stub with no real `certFacSpi` — fine for the two hand-rolled-DER-parser
/// natives (`generateCertificate`/`generateCertificates`, both check for a
/// real `certFacSpi` field first and fall back to ad-hoc parsing), but NPEs
/// on every OTHER real-bytecode-only method (`generateCertPath`,
/// `generateCRL(s)`, `getCertPathEncodings` — none natively intercepted).
/// Found root-causing `sun.security.util.SignatureFileVerifier.getSigners()`'s
/// `certificateFactory.generateCertPath(chain)` — jar-signature
/// verification's final step, `SecurityInfoTests.getWhenJarIsSigned`'s DSA
/// jar (`certificateFactory` is built via the exact 1-arg `getInstance("X509")`
/// this fixes).
///
/// Returns `None` (caller falls back to the synthetic stub) when the
/// algorithm doesn't resolve in any seeded provider.
pub(crate) fn try_build_real_certificate_factory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let algo = read_arg_string(ctx, args, 0);
    try_build_real_certificate_factory_for(ctx, None, &algo)
}

/// As above, but for a provider the caller NAMED.
///
/// `CertificateFactory.getInstance(type, providerName)` used to check that the
/// provider EXISTS and then call the one-argument form, which walks the chain —
/// so `getInstance("X.509", "BC")` handed back `SUN`'s factory. That is not
/// cosmetic: the two implementations differ in what they can DO.
/// `sun.security.provider.certpath.X509CertPath` supports the `PkiPath` and
/// `PKCS7` encodings and BouncyCastle's also supports `PEM`, so bc-java's
/// `CertPathTest` — which asks the factory it explicitly requested from BC for
/// a PEM encoding — got `CertificateEncodingException: unsupported encoding`
/// from a factory it never asked for.
pub(crate) fn try_build_real_certificate_factory_for(
    ctx: &mut dyn NativeContext,
    requested_provider: Option<&str>,
    algo: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let algo = algo.to_string();
    let provider = match requested_provider {
        Some(p) => {
            if get_service_entry(p, "CertificateFactory", &algo).is_none() {
                return Ok(None);
            }
            p.to_string()
        }
        None => match find_service_provider("CertificateFactory", &algo) {
            Some(p) => p,
            None => return Ok(None),
        },
    };
    let impl_ref = match build_jca_impl(ctx, &provider, "CertificateFactory", &algo) {
        Some(Ok(Some(Value::Object(Some(o))))) => o,
        _ => return Ok(None),
    };
    let pin = ctx.pin_native_root(impl_ref);
    let provider_obj = resolve_or_make_provider(ctx, &provider);
    let impl_ref = ctx.read_native_pin(pin, impl_ref);
    let algo_str = ctx.create_string(&algo);
    let result = ctx.new_object_initialized(
        "java/security/cert/CertificateFactory",
        "(Ljava/security/cert/CertificateFactorySpi;Ljava/security/Provider;Ljava/lang/String;)V",
        &[
            Value::Object(Some(impl_ref)),
            Value::Object(Some(provider_obj?)),
            Value::Object(Some(algo_str)),
        ],
    );
    ctx.unpin_native_roots(pin);
    match result {
        Ok(Some(Value::Object(Some(o)))) => Ok(Some(o)),
        _ => Ok(None),
    }
}

/// Bridge for providers whose real `getService()`/`Service.newInstance()`
/// never builds the SPI via reflection at all. BouncyCastle-FIPS's
/// `BouncyCastleFipsProvider` is the motivating (and, so far, only known)
/// case: `addAlgorithmImplementation` registers each algorithm through the
/// plain inherited `Provider.put(key, className)` — which we already
/// capture into `ServiceEntry.class_name` — PLUS a private
/// `Map<String, EngineCreator> creatorMap` keyed by that SAME `className`
/// string. `className` itself was never meant to be `Class.forName`-loadable
/// (BC-FIPS builds it as `<enclosing-class-dotted-name>.<memberName-with-
/// $-nesting>` purely as a label — e.g.
/// `"org.bouncycastle.jcajce.provider.ProvEC.AlgorithmParametersSpi$EC"`, a
/// string with no corresponding class file); the REAL construction is
/// `creatorMap.get(className).createInstance(param)`, which only BC-FIPS's
/// own `Provider$Service` subclass (its private `BcService`, never
/// constructed by our bridge) would ever call.
///
/// `creatorMap` has no JCA-visible accessor, so reach it directly: given the
/// real provider object, read its `creatorMap` field, look up `className`,
/// and — if a creator is on file — invoke `createInstance(Object)` on it,
/// exactly the call BC-FIPS's own bytecode would have made.
///
/// Returns `None` when `provider_obj` has no `creatorMap` field (or no
/// entry for `class_name`) — true for every ordinary
/// `className`-is-a-real-class provider — so callers fall back to the
/// existing reflective path unchanged.
fn try_engine_creator_instantiate(
    ctx: &mut dyn NativeContext,
    provider_obj: ObjectRef,
    class_name: &str,
) -> Option<MethodCallResult> {
    let creator_map = match ctx.get_field_by_name(provider_obj, "creatorMap") {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let key = ctx.create_string(class_name);
    let creator = match ctx.invoke_virtual(
        creator_map,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(key))],
    ) {
        Ok(Some(Value::Object(Some(c)))) => c,
        _ => return None,
    };
    Some(ctx.invoke_virtual(
        creator,
        "createInstance",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(None)],
    ))
}

/// `java.security.Provider$Service.newInstance(Object constructorParameter)` —
/// reflectively instantiate the entry's implementation class (a real BC `*Spi`)
/// and run its no-arg constructor, so the genuine provider bytecode produces the
/// SPI object the JDK's `GetInstance.getInstance(Service, clazz)` then wraps.
fn provider_service_new_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = {
        // Primary: GC-stable side-table lookup keyed by identity hash (header,
        // preserved across relocation — see `service_classname_table`).
        let ih = ctx.identity_hash_code(this) as i64;
        let from_id = service_classname_table().lock().get(&ih).cloned();
        let resolved = from_id
            .filter(|s| !s.is_empty())
            .or_else(|| provider_service_string_field(ctx, this, "className", 3))
            .or_else(|| provider_service_class_name_from_registry(ctx, this));
        match resolved {
            Some(s) if !s.is_empty() => s,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NotImplemented {
                    feature: "Provider$Service.newInstance with no className".into(),
                }
                .into())
            }
        }
    };
    if let Value::Object(Some(provider_obj)) = ctx.get_field_by_name(this, "provider") {
        if let Some(result) = try_engine_creator_instantiate(ctx, provider_obj, &class_name) {
            return result;
        }
    }
    let internal = class_name.replace('.', "/");
    // Engines whose SPI takes its parameters through the CONSTRUCTOR rather
    // than a no-arg ctor plus setters — see `jca_service_ctor_parameter_type`.
    // `CertStore.getInstance("Collection", params)` is the reachable case: its
    // SPI has no `()V` at all, so without this arm the arm below raised
    // `ClassNotFoundException` for a class that had loaded perfectly well.
    //
    // Only taken when a non-null `constructorParameter` was actually supplied;
    // `newInstance(null)` on such an engine keeps falling through to `()V` and
    // failing there, which is what the JDK does too (`InvalidParameterException`
    // territory, not a silently different object).
    if let Some(param_desc) = provider_service_string_field(ctx, this, "type", 0)
        .as_deref()
        .and_then(jca_service_ctor_parameter_type)
    {
        // A NULL parameter still takes this arm, which is the JDK's own rule
        // and was the half this code did not have.
        //
        // `Provider.Service.newInstance` branches on whether the ENGINE
        // declares a constructor-parameter class, not on whether the caller
        // supplied one: with a class declared it does
        // `clazz.getConstructor(ctrParamClz).newInstance(constructorParameter)`
        // and a null argument is ordinary. The previous form required a
        // non-null parameter and otherwise fell through to `()V`.
        //
        // For `CertStore` that was invisible — its `getInstance` always carries
        // parameters. `KDF.getInstance("HKDF-SHA256")` calls
        // `newInstance(null)`, and `HKDFKeyDerivation$HKDFSHA256` has no `()V`
        // at all, so the fall-through produced an object whose constructor
        // never ran: `hmacLen` read 0 and every derivation, at every length,
        // failed `length > hmacLen * 255` with "Requested length exceeds
        // maximum allowed length". Measured 2026-09-02 with
        // `apps/probes/JcaModernEngines`, whose cause-chain printing is what
        // made a message naming the PROVIDER point at the constructor.
        let param = match args.get(1).cloned() {
            Some(v @ Value::Object(Some(_))) => v,
            _ => Value::Object(None),
        };
        let ctor = format!("({param_desc})V");
        match ctx.new_object_initialized(&internal, &ctor, &[param]) {
            Ok(Some(v @ Value::Object(Some(_)))) => return Ok(Some(v)),
            Err(MethodCallFailed::ExceptionThrown(t)) => {
                return Err(MethodCallFailed::ExceptionThrown(t))
            }
            // Fall through to the no-arg attempt: a provider may have
            // registered a class under this engine that does declare `()V`.
            _ => {}
        }
    }
    // GC-safe allocate + run the no-arg constructor (real BC SPI bytecode). The
    // SPI constructor can allocate enough to trigger a moving GC, so we must not
    // hold the raw reference across `<init>` — `new_object_initialized` pins it
    // and returns the forwarded reference.
    //
    // `className` is arbitrary data the provider registered (not code we
    // control), so a lookup miss is an ordinary, expected outcome — it must
    // surface as a catchable `ClassNotFoundException`, not the non-catchable
    // internal `VmError` `new_object_initialized` raises for its normal
    // (VM-bootstrap) callers. Match on the `Result` explicitly instead of `?`
    // so a class-not-found here can't abort the process.
    match ctx.new_object_initialized(&internal, "()V", &[]) {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
        Err(MethodCallFailed::ExceptionThrown(t)) => Err(MethodCallFailed::ExceptionThrown(t)),
        _ => Err(
            cratonvm_types::error::RuntimeError::ClassNotFoundException {
                class_name: class_name.clone(),
            }
            .into(),
        ),
    }
}

/// Helper: read an argument as a Rust String (empty if null / not a String).
fn read_arg_string(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> String {
    match args.get(idx) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    }
}

#[cfg(test)]
static TEST_SERVICE_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test helper: clear the global service / alias maps and return a guard
/// that serialises tests touching global service state.  Cargo runs tests
/// in parallel by default, and the WP6.5-finish service map is process-wide;
/// without this guard, two tests can race such that test A's
/// `reset_service_state_for_tests` clears test B's entries mid-flight,
/// producing intermittent `unwrap on None` failures.  The returned guard
/// is held until the end of the calling test (bind to `_lock` — DROPPING
/// it early defeats the serialisation).
#[cfg(test)]
#[must_use = "drop the guard at the end of the test, not before; assign to `_lock`"]
fn reset_service_state_for_tests() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_SERVICE_STATE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    services().lock().clear();
    aliases().lock().clear();
    guard
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub(crate) fn register(r: &mut NativeMethodRegistry) {
    seed_direct_native_engine_services();
    let prov = "java/security/Provider";
    r.register(prov, "getName", "()Ljava/lang/String;", provider_get_name);
    r.register(prov, "getVersion", "()D", provider_get_version);
    r.register(
        prov,
        "getVersionStr",
        "()Ljava/lang/String;",
        provider_get_version_str,
    );
    r.register(prov, "toString", "()Ljava/lang/String;", provider_to_string);
    r.register(prov, "getInfo", "()Ljava/lang/String;", provider_get_info);

    // WP6.5: short-circuit `Provider$Service.<init>` so BouncyCastle's
    // setup() loop doesn't NPE on `knownEngines.get(type)` at pc=29.
    // See module-level docs above this function for rationale.
    let svc = "java/security/Provider$Service";
    r.register(
        svc,
        "<init>",
        "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
         Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V",
        provider_service_init,
    );
    // Inner-class `<clinit>` shims — `Provider$ServiceKey` and
    // `EngineDescription` real-JDK clinits would otherwise drag the
    // `knownEngines` static-init chain in.  The constructor shim above
    // makes those chains unnecessary, so no-op them to skip the
    // class-load-time work entirely.
    r.register(
        "java/security/Provider$ServiceKey",
        "<clinit>",
        "()V",
        clinit_noop,
    );
    r.register(
        "java/security/Provider$EngineDescription",
        "<clinit>",
        "()V",
        clinit_noop,
    );

    // Round 87 (WildFly): skip BouncyCastle's EC asymmetric-provider
    // configuration.  `EC.<clinit>` calls `ECNamedCurveTable.getNames()`,
    // which iterates through every named-curve table (ANSSI, GMNamedCurves,
    // ECGOST3410NamedCurves, ...).  Under the real-JDK interpreter (no JIT)
    // this walk takes ~5 minutes and then hits an operand-stack tag
    // mismatch in a downstream curve `<clinit>` ("expected long on stack,
    // got float(...)"), which the B6 silent-swallow logs but which leaves
    // BouncyCastle in a half-initialized state.  WildFly doesn't actually
    // need EC algorithms to boot; no-op the EC asymmetric provider's
    // class-init and `Mappings.configure` so `BouncyCastleProvider.setup()`
    // moves on to the next algorithm immediately.  EC support is not
    // registered, but the provider chain continues normally and downstream
    // WildFly subsystems initialize.
    // Real-JCA bring-up: when CRATONVM_REAL_JCA is set we WANT BC's EC
    // asymmetric provider to configure for real (so EC services register and
    // KeyPairGenerator/Signature("EC","BC") resolve real BC SPIs). Skip these
    // no-ops in that mode; the WildFly-era no-ops stay the default.
    //
    // 2026-08-13: they are no longer the default, because the blocker the
    // Round-87 comment above describes is gone. Re-measured on this host: with
    // the no-ops lifted, `new BouncyCastleProvider()` + all six asymmetric
    // `$Mappings.configure` calls complete in well under a second, and BC's EC
    // family registers its 343 algorithms — no ~5-minute `ECNamedCurveTable`
    // walk, no operand-stack tag mismatch. What changed since Round 87 is the
    // EC routing this VM now does by default (`route_ec_to_real`, with the
    // `sunec_intpoly`/`sunec_point` intrinsics behind it), which is also what
    // makes the curve tables cheap.
    //
    // What the no-op cost, measured the same day: `bc.getService("KeyFactory",
    // "EC")` answered `null`, so BouncyCastle could not convert an EC key at
    // all. netty's `BouncyCastlePemReader` — which netty tries BEFORE the JDK
    // parser for every PEM private key — failed with `PEMException: unable to
    // convert key pair: no such algorithm: EC for provider BC` and returned
    // null, and the JDK fallback cannot read a SEC1 `EC PRIVATE KEY` block at
    // all. That is netty `SslContextBuilderTest.
    // testCombinedPemFileClientContextJdk`'s `IllegalArgumentException: Input
    // stream does not contain valid private key.`, three layers downstream.
    //
    // The kill-switch is `CRATONVM_SYNTHETIC_EC=1` (it turns
    // `route_ec_to_real` off), which restores the Round-87 behaviour exactly.
    if !crate::real_jca_mode() && !crate::route_ec_to_real() {
        r.register(
            "org/bouncycastle/jcajce/provider/asymmetric/EC",
            "<clinit>",
            "()V",
            clinit_noop,
        );
        r.register(
            "org/bouncycastle/jcajce/provider/asymmetric/EC$Mappings",
            "<clinit>",
            "()V",
            clinit_noop,
        );
        r.register(
            "org/bouncycastle/jcajce/provider/asymmetric/EC$Mappings",
            "configure",
            "(Lorg/bouncycastle/jcajce/provider/config/ConfigurableProvider;)V",
            clinit_noop,
        );
    }

    // WP6.5 finish: service-map population + lookup.
    //
    // `Provider.put(Object,Object)` is the public surface BouncyCastle's
    // `addAlgorithm` reaches via the inherited `Hashtable` API.
    // `parseLegacyPut(String,String)` is the package-private helper that
    // older BC versions call directly; we shim both so the population
    // path is covered regardless of which one the caller uses.
    //
    // `getService(String,String)` is the consumer side — every
    // `Cipher.getInstance(algo, providerName)` /
    // `Signature.getInstance(algo, providerName)` resolution funnels
    // through it.
    r.register(
        prov,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        provider_put_native,
    );
    r.register(
        prov,
        "parseLegacyPut",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        provider_parse_legacy_put_native,
    );
    // `putService(Provider$Service)` — the modern registration surface some
    // providers (e.g. BouncyCastle's `*$Mappings.configure()`, including the
    // GOST3411 digest that originally surfaced this gap) call directly
    // instead of going through `put`/`parseLegacyPut`. Without this native,
    // the call falls through to real inherited bytecode operating on the
    // never-initialized `legacyMap`/`serviceMap` fields of our synthetic
    // `Provider` instances, which spuriously throws `IllegalStateException:
    // duplicate provider key` on ordinary re-registration (see doc comment
    // on `provider_put_service_native` above).
    r.register(
        prov,
        "putService",
        "(Ljava/security/Provider$Service;)V",
        provider_put_service_native,
    );
    r.register(
        prov,
        "getService",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;",
        provider_get_service_native,
    );
    r.register(
        prov,
        "getServices",
        "()Ljava/util/Set;",
        provider_get_services_native,
    );
    r.register(
        prov,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        provider_contains_key,
    );
    r.register(
        prov,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        provider_get_object,
    );
    // The Map surface. `java.security.Provider` IS a `Properties`, and these
    // were the methods nothing here registered — so they read the inherited
    // map while `get`/`containsKey` read the side table, and the two never
    // agreed. See `provider_map_rows`.
    r.register(prov, "size", "()I", provider_size);
    r.register(prov, "isEmpty", "()Z", provider_is_empty);
    r.register(prov, "keySet", "()Ljava/util/Set;", provider_key_set);
    r.register(prov, "entrySet", "()Ljava/util/Set;", provider_entry_set);
    r.register(prov, "values", "()Ljava/util/Collection;", provider_values);
    r.register(prov, "keys", "()Ljava/util/Enumeration;", provider_keys);
    r.register(
        prov,
        "elements",
        "()Ljava/util/Enumeration;",
        provider_elements,
    );
    r.register(
        prov,
        "stringPropertyNames",
        "()Ljava/util/Set;",
        provider_string_property_names,
    );
    r.register(prov, "putId", "()V", provider_put_id);

    // Provider$Service accessors — `getClassName()` is consumed by both
    // `Cipher.getInstance` (to instantiate the SPI) and by callers
    // building diagnostic strings.  `getType` / `getAlgorithm` /
    // `getProvider` are already registered in
    // `phases_early::register_phase53_security`; re-registering here
    // would be a no-op (idempotent) but the explicit `getClassName`
    // entry is new for WP6.5 finish.
    r.register(
        svc,
        "getClassName",
        "()Ljava/lang/String;",
        provider_service_get_class_name,
    );

    // `getAttribute` is public API, not just `supportsParameter` plumbing —
    // see `provider_service_get_attribute`. The JDK's own body reads a map
    // keyed by its private `UString` wrapper, which nothing outside
    // `java.security` can populate, so answering it here is the only way the
    // attribute rows a provider `put` become visible.
    r.register(
        svc,
        "getAttribute",
        "(Ljava/lang/String;)Ljava/lang/String;",
        provider_service_get_attribute,
    );

    r.register(
        svc,
        "toString",
        "()Ljava/lang/String;",
        provider_service_to_string,
    );

    r.register(
        "java/security/KeyStore",
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/KeyStore;",
        key_store_get_instance_with_provider,
    );

    // Real-JCA bring-up: bridge `sun.security.jca.GetInstance.getService` to our
    // provider service map, and instantiate real provider SPIs reflectively.
    // Wired whenever the real SunEC path is reachable — either full real-JCA
    // mode (`CRATONVM_REAL_JCA`) or EC-scoped default routing
    // (`route_ec_to_real`, default ON), OR DSA-scoped default routing
    // (`route_dsa_to_real`, default ON) — `AlgorithmParameters.getInstance
    // ("DSA")` needs this same bridge (see `seed_sun_dsa_services`). In
    // pure-synthetic mode (kill-switches `CRATONVM_SYNTHETIC_EC=1`/
    // `CRATONVM_SYNTHETIC_DSA=1`, both set) the key_factory/signature
    // short-circuits handle every `getInstance` and these bridges are never
    // reached.  Even when wired in default mode the bridges are EC/DSA-only in
    // practice: every other engine (`MessageDigest`/RSA/AES/…) keeps its
    // always-on synthetic native, so only real EC/DSA bytecode ever falls
    // through to here.
    let ec_real = crate::real_jca_mode() || crate::route_ec_to_real() || crate::route_dsa_to_real();
    if ec_real {
        // Mirror SunEC's EC service table into our map so the no-provider
        // `getInstance("EC")` search resolves the real pure-Java SunEC SPIs.
        seed_sunec_services();
        // Mirror the SUN provider's DSA `AlgorithmParameters` service entry
        // (see `seed_sun_dsa_services` doc comment for the root-cause story).
        seed_sun_dsa_services();
        // Mirror SunJSSE/SUN TLS service tables (KeyManagerFactory /
        // TrustManagerFactory / SSLContext / KeyStore) so the JSSE connector's
        // getInstance calls resolve real provider SPIs instead of dead-ending.
        seed_sunjsse_services();
        // Mirror the XMLDSig provider's JSR-105 DOM service table so
        // XMLSignatureFactory/KeyInfoFactory/TransformService.getInstance("DOM")
        // resolve the real DOM SPIs (keycloak SAML XMLSignatureUtil.<clinit>).
        seed_xmldsig_services();
        // Mirror the SunJCE PKCS#12 PBES2 AlgorithmParameters service table so
        // loading a password-protected PKCS12 keystore entry doesn't dead-end
        // (RestClientBuilderIntegTests HTTPS suite-timeout).
        seed_sunjce_pbe_services();
        // SunJCE's thirty `SecretKeyFactory` services, of which twenty-eight
        // were already SERVED and none were advertised, and the two `Algorithm-
        // ParameterGenerator` services, which were neither.
        seed_sunjce_secret_key_factory_services();
        seed_algorithm_parameter_generator_services();
        seed_sunjce_pbe_mac_services();
        seed_sunjce_delegated_cipher_services();
        seed_sunjce_modern_engine_services();
        let gi = "sun/security/jca/GetInstance";
        r.register(
            gi,
            "getService",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;",
            getinstance_get_service_provider,
        );
        r.register(
            gi,
            "getService",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;",
            getinstance_get_service_search,
        );
        r.register(
            svc,
            "newInstance",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            provider_service_new_instance,
        );
        // Preferred path: intercept GetInstance.getInstance directly and return
        // a fully-built Instance (provider + real SPI impl). This avoids the
        // Provider$Service object round-trip whose raw-slot className storage is
        // not GC-stable. Covers the String-provider, Provider-object, and
        // no-provider overloads used by KeyPairGenerator/KeyFactory/Signature/
        // Cipher/MessageDigest.getInstance.
        r.register(
            gi,
            "getInstance",
            "(Ljava/lang/String;Ljava/lang/Class;Ljava/lang/String;Ljava/lang/String;)Lsun/security/jca/GetInstance$Instance;",
            getinstance_instance_provider,
        );
        r.register(
            gi,
            "getInstance",
            "(Ljava/lang/String;Ljava/lang/Class;Ljava/lang/String;Ljava/security/Provider;)Lsun/security/jca/GetInstance$Instance;",
            getinstance_instance_provider_obj,
        );
        r.register(
            gi,
            "getInstance",
            "(Ljava/lang/String;Ljava/lang/Class;Ljava/lang/String;)Lsun/security/jca/GetInstance$Instance;",
            getinstance_instance_search,
        );
        // Lazy serviceIterator path (KeyFactory/Signature/Cipher constructors).
        r.register(
            gi,
            "getServices",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/util/Iterator;",
            getinstance_get_services,
        );
    }

    // BouncyCastle X509/cert-parsing extras. These were previously gated to the
    // full real-JCA path, but the bug they fix also bites DEFAULT mode: every
    // provider we hand out is a synthetic allocated by `make_provider` (the JDK
    // `Provider` constructor never runs, so the inherited `initialized` boolean
    // is false). BC's `X509SignatureUtil.lookupAlg` -> `Security.getProvider(
    // "BC").getProperty("Alg.Alias.Signature.OID.<oid>")` then hits the real
    // `Provider.getProperty`, whose `checkInitialized()` throws a bare
    // `IllegalStateException`, which BC rewraps as `CertificateParsingException:
    // cannot construct SigAlgName` — failing ALL BC X.509 cert parsing (keycloak
    // PemUtils/cert/RSAVerifier suites). Registering our `getProperty` override
    // unconditionally returns the captured put/alias value (or null, which BC
    // handles via its `getId()` fallback) and never touches `checkInitialized`.
    // Safe in default mode: the real path is already broken for synthetic
    // providers, and the EC-scoped real-SunEC routing never calls getProperty.
    r.register(
        prov,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        provider_get_property,
    );
    // KEEP (correct constant, not a stub): this reports a CAPABILITY, not a
    // security decision — nothing is bypassed by answering it.
    //
    // jdk.internal.event.EventHelper.isLoggingSecurity() — the security-event
    // logging gate. Its real body dereferences the static `JUJA`
    // (`SharedSecrets.getJavaUtilJarAccess()`), which is null in our VM, so it
    // NPEs ("Cannot invoke isInitializing on null") on the
    // CertificateFactory.generateCertificate -> JCAUtil.tryCommitCertEvent
    // path.
    //
    // VERIFIED against jdk-25 bytecode (`javap -c jdk.internal.event
    // .EventHelper`): the real body lazily installs `System.getLogger(
    // "jdk.event.security")` and stores `logger.isLoggable(LOG_LEVEL)` — i.e.
    // it is a readout of whether DEBUG logging is enabled for that one logger
    // name, which on a stock JDK with no logging configuration is FALSE. So
    // `false` is both the real JDK's default answer and factually true here
    // (CratonVM emits no security events at all). Answering `true` would be
    // actively harmful: the caller would then go on to `logSecurityEvent`,
    // straight back into the same null `JUJA`.
    r.register(
        "jdk/internal/event/EventHelper",
        "isLoggingSecurity",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );

    let sec = "java/security/Security";
    r.register(
        sec,
        "getProviders",
        "()[Ljava/security/Provider;",
        security_get_providers,
    );
    r.register(
        sec,
        "getProviders",
        "(Ljava/lang/String;)[Ljava/security/Provider;",
        security_get_providers_filtered,
    );
    r.register(
        sec,
        "getProvider",
        "(Ljava/lang/String;)Ljava/security/Provider;",
        security_get_provider,
    );
    r.register(
        sec,
        "addProvider",
        "(Ljava/security/Provider;)I",
        security_add_provider,
    );
    r.register(
        sec,
        "insertProviderAt",
        "(Ljava/security/Provider;I)I",
        security_insert_provider_at,
    );
    r.register(
        sec,
        "removeProvider",
        "(Ljava/lang/String;)V",
        security_remove_provider,
    );
    r.register(
        sec,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        security_get_property,
    );
    r.register(
        sec,
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        security_set_property,
    );
    // W4-3: without this, real-JDK mode ran the JDK's own `getAlgorithms`
    // bytecode, which reads `provider.keys()` off the synthetic Providers
    // `make_provider` hands out — an empty Hashtable, so EVERY engine type
    // answered the empty set. See `algorithms_for_service`.
    r.register(
        sec,
        "getAlgorithms",
        "(Ljava/lang/String;)Ljava/util/Set;",
        security_get_algorithms,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// The seed chain is PER-PLATFORM: `SunMSCAPI` wraps the Windows CryptoAPI
    /// and ships only in the Windows JDK, so HotSpot 25 answers thirteen
    /// providers there and twelve everywhere else. This test asserted a flat
    /// thirteen and so encoded a Windows capture as universal — measured
    /// against `java -version 25.0.3` on Linux, which lists exactly the twelve
    /// below.
    #[test]
    fn seed_chain_matches_the_platform_jdk25_provider_list() {
        let chain = snapshot();
        let names: Vec<&str> = chain.iter().map(|(n, _, _)| n.as_str()).collect();
        let mut expected = vec![
            "SUN",
            "SunRsaSign",
            "SunEC",
            "SunJSSE",
            "SunJCE",
            "SunJGSS",
            "SunSASL",
            "XMLDSig",
            "SunPCSC",
            "JdkLDAP",
            "JdkSASL",
        ];
        if cfg!(target_os = "windows") {
            expected.push("SunMSCAPI");
        }
        expected.push("SunPKCS11");
        // The whole ordered list, not a length plus three spot-checks: order is
        // what provider selection walks, so a chain that is the right length
        // with two entries swapped resolves algorithms to the wrong provider
        // and still passes a spot-check.
        assert_eq!(names, expected);
    }

    // -----------------------------------------------------------------
    // C19 — coverage disclosure for unbacked providers.
    // -----------------------------------------------------------------

    #[test]
    fn c19_seed_chain_marks_unbacked_providers() {
        // Only providers with no registered service surface stay placeholders.
        for unbacked in [
            "SunPKCS11",
            "SunPCSC",
            "JdkLDAP",
            "JdkSASL",
            "SunJGSS",
            "SunSASL",
            "XMLDSig",
        ] {
            assert!(
                is_unbacked_provider(unbacked),
                "{unbacked} must be flagged as unbacked so getService can log a hint"
            );
            let (_, cov) = find(unbacked).expect("seed provider present");
            assert_eq!(
                cov, COVERAGE_UNBACKED,
                "{unbacked} coverage string must be the canonical unbacked-disclosure constant"
            );
        }
        // `SunMSCAPI` is deliberately absent from the seed chain off Windows
        // (it wraps CryptoAPI and ships only in the Windows JDK), so asserting
        // it is present would be a gate that fails on the platform where the
        // right answer is "not there".
        let mut backed_names = vec!["SUN", "SunRsaSign", "SunJCE", "SunEC", "SunJSSE"];
        if cfg!(target_os = "windows") {
            backed_names.push("SunMSCAPI");
        } else {
            assert!(
                find("SunMSCAPI").is_none(),
                "SunMSCAPI must NOT be seeded off Windows: HotSpot's own \
                 Security.getProviders() does not carry it there"
            );
        }
        for backed in backed_names {
            assert!(
                !is_unbacked_provider(backed),
                "{backed} backs at least one algorithm and must not be flagged unbacked"
            );
            let (_, cov) = find(backed).expect("seed provider present");
            assert!(
                cov.starts_with("coverage: ") && cov != COVERAGE_UNBACKED,
                "{backed} coverage must describe what is actually wired, got: {cov}"
            );
        }
    }

    #[test]
    fn c19_user_added_provider_carries_user_coverage_string() {
        // `Security.addProvider(...)` lands here; we don't introspect
        // the caller's Service map, so the disclosure says exactly that.
        let name = format!("__jca_test_user_cov_{}", std::process::id());
        let pos = add(name.clone(), 7.5);
        assert!(pos >= 1);
        let (ver, cov) = find(&name).expect("user-added provider visible to find()");
        assert_eq!(ver, 7.5);
        assert_eq!(cov, USER_PROVIDER_COVERAGE);
        // User-added providers are NOT seed unbacked entries — the
        // unbacked flag is set by string-equality against
        // COVERAGE_UNBACKED, so USER_PROVIDER_COVERAGE must not match.
        assert!(!is_unbacked_provider(&name));
        remove(&name);
    }

    #[test]
    fn add_provider_idempotent() {
        let before = snapshot().len();
        // Adding an existing name must not duplicate.
        let pos = add("SUN".to_string(), 25.0);
        assert_eq!(pos, 1, "SUN is at position 1 after re-add");
        assert_eq!(snapshot().len(), before);
    }

    #[test]
    fn insert_then_remove_round_trip() {
        let unique_name = format!("__jca_test_{}", std::process::id());
        let pos = insert_at(unique_name.clone(), 42.0, 5);
        assert!(pos >= 1);
        assert!(find(&unique_name).is_some());
        remove(&unique_name);
        assert!(find(&unique_name).is_none());
    }

    // -----------------------------------------------------------------
    // WP6.5: Provider$Service.<init> shim — verify the constructor
    // populates all six instance fields without invoking real bytecode
    // (no `knownEngines.get` lookup, no NPE).
    // -----------------------------------------------------------------

    use crate::test_utils::MockNativeContext;

    // -----------------------------------------------------------------
    // G43-1 — `make_provider`'s legacy slot mirror is synthetic-mode-only.
    //
    // The two arms below are the whole decision. Under `--jdk-only` the
    // receiver is a real `java.security.Provider`, whose slots 0/1/2 are the
    // inherited `Hashtable.table` / `count` / `threshold`; the slot-2 write
    // published a heap ADDRESS into an `I` field, MEASURED twice per `RCrypto`
    // run by `CRATONVM_DBG_COERCION=1` as species `pointer-into-primitive` at
    // `provider_chain.rs:317`. Under `--synthetic-jdk` the same three slots
    // ARE name/version/info and `provider_get_name` / `provider_get_version` /
    // `provider_get_info` read them.
    // -----------------------------------------------------------------

    /// Declare `java.security.Provider`'s OWN instance fields at slots that do
    /// not overlap the legacy mirror, which is what a real-JDK layout looks
    /// like from a native's point of view: the named writes resolve, and 0/1/2
    /// belong to somebody else (`Hashtable`).
    fn declare_real_provider_layout(ctx: &MockNativeContext, cid: cratonvm_types::ClassId) {
        let fields = ["name", "info", "version", "versionStr", "initialized"]
            .iter()
            .enumerate()
            .map(|(i, n)| cratonvm_native_api::FieldMetadata {
                name: (*n).to_string(),
                descriptor: "Ljava/lang/Object;".to_string(),
                access_flags: 0,
                // Slot 3 upward — 0/1/2 are the inherited Hashtable fields.
                slot_index: i + 3,
                declaring_class_id: cid,
                is_static: false,
            })
            .collect();
        ctx.set_declared_fields(cid, fields);
    }

    #[test]
    fn g43_1_make_provider_writes_the_legacy_slots_only_in_synthetic_mode() {
        let mut ctx = MockNativeContext::new();
        // No declared layout → every `set_field_by_name` for a Provider-only
        // name is a no-op, which is exactly what production does when the real
        // class has no bytes. `versionStr` cannot read back, so the mirror runs.
        let p = make_provider(&mut ctx, "SUN", 25.0, "test coverage").expect("alloc");

        assert!(
            matches!(ctx.get_field(p, 0), Value::Object(Some(_))),
            "synthetic slot 0 must carry the name String (provider_get_name reads it)"
        );
        assert_eq!(
            ctx.get_field(p, 1),
            Value::Double(25.0),
            "synthetic slot 1 must carry the numeric version (provider_get_version reads it)"
        );
        assert!(
            matches!(ctx.get_field(p, 2), Value::Object(Some(_))),
            "synthetic slot 2 must carry the info String (provider_get_info reads it)"
        );
        // The two Strings are distinct objects — a mirror that wrote the same
        // ref twice would satisfy the two assertions above vacuously.
        assert_ne!(
            ctx.get_field(p, 0),
            ctx.get_field(p, 2),
            "name and info must be different objects"
        );
    }

    #[test]
    fn g43_1_make_provider_does_not_publish_an_address_into_a_real_provider_int_slot() {
        let mut ctx = MockNativeContext::new();
        let cid = ctx
            .ensure_class_initialized("java/security/Provider")
            .expect("mock registers the class");
        declare_real_provider_layout(&ctx, cid);

        let p = make_provider(&mut ctx, "SUN", 25.0, "test coverage").expect("alloc");

        // The named writes landed where the class says they go...
        assert!(
            matches!(ctx.get_field_by_name(p, "name"), Value::Object(Some(_))),
            "real-layout `name` must be written by name"
        );
        assert!(
            matches!(
                ctx.get_field_by_name(p, "versionStr"),
                Value::Object(Some(_))
            ),
            "real-layout `versionStr` must be written by name"
        );
        assert!(
            matches!(ctx.get_field_by_name(p, "info"), Value::Object(Some(_))),
            "real-layout `info` must be written by name"
        );

        // ...and the legacy mirror did NOT run. Slot 2 is `Hashtable.threshold:I`
        // on a real Provider; a reference there is the `pointer-into-primitive`
        // coercion, and slot 0 is `Hashtable.table`, whose occupant a live
        // `Hashtable.keys()` walks as an `Entry[]`.
        assert!(
            !matches!(ctx.get_field(p, 2), Value::Object(Some(_))),
            "slot 2 is Hashtable.threshold on a real Provider — writing a \
             reference there is the pointer-into-primitive coercion this fixes"
        );
        assert!(
            !matches!(ctx.get_field(p, 0), Value::Object(Some(_))),
            "slot 0 is Hashtable.table on a real Provider — a String there is \
             walked as an Entry[] by any live keys()/entrySet()"
        );
        assert_ne!(
            ctx.get_field(p, 1),
            Value::Double(25.0),
            "slot 1 is Hashtable.count on a real Provider — a Double there \
             decodes to the low half of its IEEE-754 bit pattern"
        );
    }

    #[test]
    fn g43_1_provider_get_info_never_returns_a_primitive_from_the_slot_fallback() {
        // `provider_get_info`'s descriptor is `()Ljava/lang/String;`. On a real
        // Provider slot 2 is an `int`, and an unwritten reference slot reads
        // back as `Int(0)` besides — either way, handing the interpreter an
        // `Int` from a reference-returning native is a type error at the call
        // site, so the fallback must degrade to null instead.
        let mut ctx = MockNativeContext::new();
        let p = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(p, 2, Value::Int(12));

        let got = provider_get_info(&mut ctx, &[Value::Object(Some(p))])
            .expect("getInfo must not fail")
            .expect("getInfo returns a value");
        assert_eq!(
            got,
            Value::Object(None),
            "an `I` slot must not become a String"
        );

        // The reference case still passes through unchanged.
        let info = ctx.create_string("SUN security provider");
        ctx.set_field(p, 2, Value::Object(Some(info)));
        let got = provider_get_info(&mut ctx, &[Value::Object(Some(p))])
            .expect("getInfo must not fail")
            .expect("getInfo returns a value");
        assert_eq!(got, Value::Object(Some(info)));
    }

    /// Helper: allocate a synthetic Provider$Service heap entry sized
    /// large enough for the slot fallback (synthetic getters at 0/1/2)
    /// plus three more slots so `set_field_by_name` calls the production
    /// code makes don't get rejected by `mock_jdk_field_slot` for unknown
    /// field names (the mock silently ignores unknown names).
    fn alloc_service(ctx: &mut MockNativeContext) -> ObjectRef {
        // Synthetic mode: alloc 7 slots (0..6) so writes via set_field
        // never overflow.
        ctx.alloc_object(cratonvm_types::ClassId::new(0), 7)
    }

    #[test]
    fn wp6_5_provider_service_init_registered_with_jdk21_descriptor() {
        // Acceptance pin #1: the `<init>` shim must be registered on the
        // exact 6-arg descriptor that BouncyCastle's `addAlgorithm` call
        // chain reaches via `parseLegacyPut`. A descriptor mismatch would
        // silently fall through to the real-JDK bytecode and re-trigger
        // the `knownEngines.get(type)` NPE at pc=29.
        let mut r = NativeMethodRegistry::new();
        register(&mut r);
        let cb = r.find(
            "java/security/Provider$Service",
            "<init>",
            "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
             Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V",
        );
        assert!(
            cb.is_some(),
            "Provider$Service.<init> with the JDK 21+ 6-arg descriptor must be registered"
        );

        // Defence: register() must be idempotent — re-registering the
        // same triple from a sibling call site would otherwise panic on
        // a hash collision check.
        register(&mut r);
        assert!(r
            .find(
                "java/security/Provider$Service",
                "<init>",
                "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
                 Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V",
            )
            .is_some());
    }

    #[test]
    fn wp6_5_provider_service_init_populates_synthetic_slots() {
        // The synthetic-mode `Provider$Service` getters declared in
        // `phases_early::register_phase53_security` read from slots
        // 0/1/2 → (type, algorithm, provider). Verify the shim writes
        // those slots so subsequent `getType()` / `getAlgorithm()` /
        // `getProvider()` calls return the right references.
        let mut ctx = MockNativeContext::new();
        let this = alloc_service(&mut ctx);
        let provider = ctx.create_string("BC");
        let svc_type = ctx.create_string("MessageDigest");
        let algorithm = ctx.create_string("SHA-256");
        let class_name = ctx.create_string("org.bouncycastle.jcajce.provider.digest.SHA256$Digest");
        let aliases = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
        let attributes = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);

        let args = [
            Value::Object(Some(this)),
            Value::Object(Some(provider)),
            Value::Object(Some(svc_type)),
            Value::Object(Some(algorithm)),
            Value::Object(Some(class_name)),
            Value::Object(Some(aliases)),
            Value::Object(Some(attributes)),
        ];
        let res = provider_service_init(&mut ctx, &args);
        assert!(res.is_ok(), "shim must not error");
        assert!(matches!(res.unwrap(), None), "constructor returns void");

        // Synthetic-mode slot mirror: type=0, algorithm=1, provider=2.
        assert!(
            matches!(ctx.get_field(this, 0), Value::Object(Some(o)) if o == svc_type),
            "slot 0 must hold the `type` argument (synthetic getType())"
        );
        assert!(
            matches!(ctx.get_field(this, 1), Value::Object(Some(o)) if o == algorithm),
            "slot 1 must hold the `algorithm` argument (synthetic getAlgorithm())"
        );
        assert!(
            matches!(ctx.get_field(this, 2), Value::Object(Some(o)) if o == provider),
            "slot 2 must hold the `provider` argument (synthetic getProvider())"
        );
        assert!(
            matches!(ctx.get_field(this, 3), Value::Object(Some(o)) if o == class_name),
            "slot 3 must hold the `className` argument (synthetic getClassName())"
        );
        let ih = ctx.identity_hash_code(this) as i64;
        assert_eq!(
            service_classname_table()
                .lock()
                .get(&ih)
                .map(String::as_str),
            Some("org.bouncycastle.jcajce.provider.digest.SHA256$Digest")
        );
    }

    #[test]
    fn wp6_5_provider_service_init_does_not_npe_on_null_engines() {
        // The whole point of the shim: the constructor must NOT consult
        // `knownEngines` (which would NPE because the `Provider.<clinit>`
        // chain doesn't fully populate it under real-JDK class loading
        // here). Calling the shim with all-null aux arguments — the
        // pathological case BouncyCastle's `parseLegacyPut` produces
        // when an attribute map is empty — must return Ok(None) without
        // touching any static state.
        let mut ctx = MockNativeContext::new();
        let this = alloc_service(&mut ctx);
        let svc_type = ctx.create_string("Cipher");
        let algorithm = ctx.create_string("AES/GCM/NoPadding");

        let args = [
            Value::Object(Some(this)),
            Value::Object(None), // null provider
            Value::Object(Some(svc_type)),
            Value::Object(Some(algorithm)),
            Value::Object(None), // null className
            Value::Object(None), // null aliases
            Value::Object(None), // null attributes
        ];
        let res = provider_service_init(&mut ctx, &args);
        assert!(
            res.is_ok(),
            "shim must accept null aux args (parseLegacyPut produces these)"
        );

        // type and algorithm must still land in the synthetic slots.
        assert!(matches!(ctx.get_field(this, 0), Value::Object(Some(o)) if o == svc_type));
        assert!(matches!(ctx.get_field(this, 1), Value::Object(Some(o)) if o == algorithm));
        // Provider slot is null — that's OK, the BC chain only consults
        // `getType` / `getAlgorithm` for the cache key.
        assert!(matches!(ctx.get_field(this, 2), Value::Object(None)));
    }

    #[test]
    fn wp6_5_provider_service_init_throws_on_null_this() {
        // Defensive: a null receiver must surface a NullPointerException
        // (via `obj_arg`'s contract), not panic or silently succeed.
        // BouncyCastle never produces this case but a regression in
        // bytecode dispatch could hand us a null `this`.
        let mut ctx = MockNativeContext::new();
        let args = [
            Value::Object(None), // null this
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
        ];
        let res = provider_service_init(&mut ctx, &args);
        assert!(
            res.is_err(),
            "null `this` must produce a NullPointerException, not silently succeed"
        );
    }

    #[test]
    fn wp6_5_inner_class_clinits_registered() {
        // The inner-class clinit no-ops are part of the same fix:
        // without them, the JVM walks `Provider$ServiceKey.<clinit>` and
        // `Provider$EngineDescription.<clinit>` whose real bytecode
        // touches the same `knownEngines` map indirectly. No-opping
        // them keeps the fix self-consistent.
        let mut r = NativeMethodRegistry::new();
        register(&mut r);
        assert!(
            r.find("java/security/Provider$ServiceKey", "<clinit>", "()V")
                .is_some(),
            "Provider$ServiceKey.<clinit> must be no-op'd alongside the Service ctor shim"
        );
        assert!(
            r.find(
                "java/security/Provider$EngineDescription",
                "<clinit>",
                "()V"
            )
            .is_some(),
            "Provider$EngineDescription.<clinit> must be no-op'd alongside the Service ctor shim"
        );
    }

    // -----------------------------------------------------------------
    // WP6.5 finish — service map population + lookup.
    //
    // These tests pin the unit-level behaviour of the parseLegacyPut
    // and getService chain: that "Cipher.AES/GCM/NoPadding"-style keys
    // crack into (Cipher, AES/GCM/NoPadding), that the entry lands in
    // the per-provider service map, and that getService returns it
    // case-insensitively.  Higher-level integration is in
    // `vm/tests/wp6_5_finish_provider_chain_resolution.rs`.
    //
    // Each test calls `reset_service_state_for_tests()` first so global
    // state from sibling tests doesn't bleed in.
    // -----------------------------------------------------------------

    #[test]
    fn parse_legacy_key_primary_form() {
        let parsed = parse_legacy_key("Cipher.AES/GCM/NoPadding").unwrap();
        assert_eq!(parsed.0, "primary");
        assert_eq!(parsed.1, "Cipher");
        assert_eq!(parsed.2, "AES/GCM/NoPadding");
        assert!(parsed.3.is_none());
    }

    #[test]
    fn parse_legacy_key_alias_form() {
        let parsed = parse_legacy_key("Alg.Alias.Cipher.AES").unwrap();
        assert_eq!(parsed.0, "alias");
        assert_eq!(parsed.1, "Cipher");
        assert_eq!(parsed.2, "AES");
        assert!(parsed.3.is_none());
    }

    #[test]
    fn parse_legacy_key_attribute_form() {
        // "Type.Algorithm AttrName" — attr lands in the optional 4th tuple slot.
        let parsed = parse_legacy_key("Cipher.AES SupportedModes").unwrap();
        assert_eq!(parsed.0, "attr");
        assert_eq!(parsed.1, "Cipher");
        assert_eq!(parsed.2, "AES");
        assert_eq!(parsed.3.as_deref(), Some("SupportedModes"));
    }

    #[test]
    fn parse_legacy_key_rejects_non_service_keys() {
        // Empty + dot-only strings must not parse — they'd otherwise
        // pollute the service map with bogus entries.
        assert!(parse_legacy_key("").is_none());
        assert!(parse_legacy_key(".").is_none());
        // A no-dot key (no Type.Algorithm separator) must reject.
        assert!(parse_legacy_key("UnstructuredKey").is_none());
        // An incomplete alias ("Alg.Alias.Cipher" with no third segment)
        // must reject — the OpenJDK helper requires both type and alias.
        assert!(parse_legacy_key("Alg.Alias.Cipher").is_none());
    }

    #[test]
    fn put_service_round_trip_via_apply_legacy_put() {
        let _lock = reset_service_state_for_tests();
        let ok = apply_legacy_put(
            "TestProvider",
            "Cipher.AES/GCM/NoPadding",
            "com.example.TestAesGcm",
        );
        assert!(ok, "primary key must populate the service map");
        let entry = get_service_entry("TestProvider", "Cipher", "AES/GCM/NoPadding").unwrap();
        assert_eq!(entry.type_str, "Cipher");
        assert_eq!(entry.algorithm, "AES/GCM/NoPadding");
        assert_eq!(entry.class_name, "com.example.TestAesGcm");
    }

    #[test]
    fn get_service_entry_is_case_insensitive() {
        let _lock = reset_service_state_for_tests();
        apply_legacy_put("BC", "Cipher.AES/GCM/NoPadding", "org.bc.AESGCM");
        // Lower-case lookup must hit.
        assert!(get_service_entry("BC", "cipher", "aes/gcm/nopadding").is_some());
        // Mixed case must hit.
        assert!(get_service_entry("BC", "CIPHER", "AES/GCM/nopadding").is_some());
        // Wrong provider name does NOT hit (provider names are stable).
        assert!(get_service_entry("SUN", "Cipher", "AES/GCM/NoPadding").is_none());
    }

    #[test]
    fn direct_native_engine_seed_tracks_real_provider_ownership() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        assert!(get_service_entry("SunRsaSign", "KeyFactory", "RSA").is_some());
        assert!(get_service_entry("SUN", "KeyFactory", "RSA").is_none());
        assert!(get_service_entry("SunRsaSign", "Signature", "SHA256withRSA").is_some());
        assert!(get_service_entry("SUN", "SecureRandom", "SHA1PRNG").is_some());
        assert!(get_service_entry("SunJCE", "Cipher", "AES").is_some());
        assert!(get_service_entry("SUN", "Cipher", "AES").is_none());
        assert!(get_service_entry("SunJCE", "Cipher", "AESWrap").is_some());
    }

    /// W7-15 ratchet: every `Cipher` transformation this provider ADVERTISES
    /// must be one `jca::cipher` can actually COMPUTE.
    ///
    /// The census that opened this lane found the two lists disagreeing in both
    /// directions simultaneously — `ChaCha20` / `ChaCha20-Poly1305` advertised
    /// and silently served as AES-256-ECB, `AES/KW/PKCS5Padding` and
    /// `AES/KWP/NoPadding` advertised and served by nothing, `DES` / `DESede`
    /// computed correctly and never advertised at all. Both directions are
    /// defects and only one of them is loud: an advertised-but-absent algorithm
    /// raises at `getInstance`, while a served-but-unadvertised one is
    /// whatever the default arm felt like doing.
    ///
    /// A census run by hand drifts again by the next wave. This is the same
    /// closure the bridge-wave population used: the measurement becomes a test,
    /// so the next person to add a name to either list has to add it to both.
    #[test]
    fn every_advertised_sunjce_cipher_is_serviceable() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        // The second seeder, for the same reason the `Mac` ratchet calls its
        // own: a population read out of the registry only covers what the
        // seeders this test CALLS have put there.
        seed_sunjce_delegated_cipher_services();
        let advertised: Vec<String> = services()
            .lock()
            .get("SunJCE")
            .expect("the SunJCE seed must have run")
            .values()
            .filter(|e| e.type_str == "Cipher")
            .map(|e| e.algorithm.clone())
            .collect();
        assert!(
            advertised.len() >= 12,
            "the SunJCE Cipher seed looks empty: {advertised:?}"
        );
        for algorithm in &advertised {
            // SERVICEABLE is a disjunction, and the second arm is what the
            // seventeen delegated transformations rest on.
            //
            // `transformation_is_serviceable` asks whether THIS engine computes
            // the name. `Cipher.getInstance`'s anonymous overload does not stop
            // there: on a refusal it walks the chain and instantiates the class
            // the service row names, which is the whole implementation for a
            // name with no Rust engine (PBES2 at the PRFs `pbes2_aes_params`
            // has no arm for, the PKCS#12 PBE family, `DESedeWrap`, `RC2`, and
            // the six AES key-wrap paddings).
            //
            // The second arm has to be a REAL class. A row carrying the
            // `.Native` MARKER is not one — it means "a Rust engine answers
            // this" — and a marker row for a name the engine does not compute
            // is exactly the W7-15 defect. It is also why
            // `jca-provider-population-gap-20260830.md` §5.2 concluded a
            // service row could never be sufficient: the six rows it added
            // carried the marker.
            let routed = get_service_entry("SunJCE", "Cipher", algorithm)
                .map(|e| {
                    let c = e.class_name.trim().to_string();
                    !c.is_empty() && !c.ends_with(".Native")
                })
                .unwrap_or(false);
            assert!(
                crate::jca::cipher::transformation_is_serviceable(algorithm) || routed,
                "SunJCE advertises Cipher.{algorithm}, but Cipher.getInstance refuses it — \
                 advertising an algorithm the engine cannot compute is the defect W7-15 closed"
            );
        }
        // And the reverse direction for the names that were the actual bug.
        // They were removed on 2026-08-11 while unimplemented and put back the
        // same day once implemented, so what this asserts is the INVARIANT —
        // advertised and computable are one set — rather than a fixed verdict
        // about these four names. The loop above already proves each is
        // serviceable; this proves the seed did not quietly drop them.
        for implemented in [
            "ChaCha20",
            "ChaCha20-Poly1305",
            "AES/KW/PKCS5Padding",
            "AES/KWP/NoPadding",
            // W7-39, 2026-08-12: same history, one wave later. Both were pulled
            // from the seed on 08-11 while `getInstance` served them as
            // AES-128-ECB, and both are back now that `jca::cipher::
            // drive_real_ecb_cipher` computes them through the real SunJCE SPI.
            "ARCFOUR",
            "Blowfish",
        ] {
            assert!(
                get_service_entry("SunJCE", "Cipher", implemented).is_some(),
                "{implemented} is implemented but no longer advertised: the two lists have drifted, which is the defect this test exists for"
            );
            assert!(
                crate::jca::cipher::transformation_is_serviceable(implemented),
                "{implemented} is advertised but Cipher.getInstance refuses it"
            );
        }
        // An ALIAS is resolvable without being advertised — that is the whole
        // point of one — so `RC4` has to be checked through the alias map rather
        // than through the advertised list, which is where the loop above would
        // never have looked. Measured on HotSpot 25: `Cipher.getInstance("RC4")`
        // resolves, answers `getProvider()=SunJCE`, and produces the same bytes
        // as `ARCFOUR`, while `Security.getAlgorithms("Cipher")` names only
        // ARCFOUR.
        //
        // `TripleDES` is deliberately NOT in this loop. It resolves through the
        // alias map like the two below, but `Cipher.getInstance("TripleDES")` is
        // still refused, because SunJCE maps it to the BARE `DESede` and a bare
        // DESede defaults to ECB — a mode this engine does not route. That is a
        // real divergence from HotSpot and it predates W7-39; asserting
        // serviceability for it here would red the test for a gap this lane did
        // not open and does not close.
        for (alias, canonical) in [("RC4", "ARCFOUR"), ("AESWrap", "AES/KW/NoPadding")] {
            assert!(
                get_service_entry("SunJCE", "Cipher", alias).is_some(),
                "{alias} must resolve through the alias map to {canonical}"
            );
            assert!(
                !advertised.iter().any(|a| a.eq_ignore_ascii_case(alias)),
                "{alias} is an ALIAS and must stay out of Security.getAlgorithms, \
                 which is where HotSpot keeps it"
            );
            assert!(
                crate::jca::cipher::transformation_is_serviceable(alias),
                "{alias} resolves as a service but Cipher.getInstance refuses it"
            );
        }
    }

    /// Collect every algorithm `provider` advertises for `type_str` straight
    /// out of the seed map.
    ///
    /// Deliberately not via `algorithms_for_service`: that walks the live
    /// PROVIDER CHAIN, and the chain's coverage here is not what the seed
    /// wrote, so a chain-based read could answer a shorter set and the loops
    /// below would pass by iterating less than they think. Each caller also
    /// asserts a minimum row count before looping, because a loop over nothing
    /// is a probe that cannot fail.
    fn advertised_for(provider: &str, type_str: &str) -> Vec<String> {
        services()
            .lock()
            .get(provider)
            .map(|m| {
                m.values()
                    .filter(|e| e.type_str == type_str)
                    .map(|e| e.algorithm.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every `(provider, algorithm)` the seed advertises for `type_str`,
    /// across EVERY provider in the map.
    ///
    /// The hardcoded provider list this replaces would have missed the
    /// `SunJCE` `KeyFactory` `ML-KEM` umbrella entirely — an
    /// advertised-and-refused name that neither originating record lists and
    /// that only the census found. A ratchet that checks a subset of the
    /// population is a ratchet with a hole in exactly the place nobody is
    /// looking.
    fn all_advertised(type_str: &str) -> Vec<(String, String)> {
        let providers: Vec<String> = services().lock().keys().cloned().collect();
        let mut out = Vec::new();
        for provider in providers {
            for algorithm in advertised_for(&provider, type_str) {
                out.push((provider.clone(), algorithm));
            }
        }
        out
    }

    /// W7-63 ratchet: every `MessageDigest` name `SUN` advertises must be one
    /// `MessageDigest.getInstance` will serve, and every digest this VM
    /// implements must be advertised.
    ///
    /// Both directions, because this pair drifted in both at once and only one
    /// of them was loud. `MD2` sat in the advertised array for three waves
    /// while `algorithm_supported` refused it — three lines above a comment
    /// declining to advertise SHAKE for exactly that reason. `SHAKE128-256`
    /// and `SHAKE256-512` were the quiet direction: correctly withheld while
    /// unimplemented, and then still withheld after the dependency to
    /// implement them was already in the tree. A census run by hand finds one
    /// of those and drifts again by the next wave; a test finds both and
    /// cannot.
    #[test]
    fn every_advertised_sun_message_digest_is_serviceable() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        let advertised = all_advertised("MessageDigest");
        assert!(
            advertised.len() >= 15,
            "the MessageDigest seed looks empty: {advertised:?}"
        );
        for (provider, algorithm) in &advertised {
            assert!(
                crate::jca::message_digest::algorithm_supported_public(algorithm),
                "{provider} advertises MessageDigest.{algorithm}, but \
                 MessageDigest.getInstance refuses it — advertising a digest the engine \
                 cannot compute is the defect W7-63 closed"
            );
        }
        // The reverse direction. These are the three names the two records
        // were actually about; the loop above proves each is serviceable, this
        // proves the seed did not quietly drop it.
        for implemented in ["MD2", "SHAKE128-256", "SHAKE256-512"] {
            assert!(
                get_service_entry("SUN", "MessageDigest", implemented).is_some(),
                "{implemented} is implemented but not advertised: the two lists have drifted, \
                 which is the defect this test exists for"
            );
        }
        // And the ALIAS half, which decides whether the advertised count lands
        // on HotSpot's 15 or on 17. `SHAKE128` / `SHAKE256` resolve through
        // `getInstance` (measured on HotSpot 25: byte-identical digests to the
        // hyphenated primaries) while `Security.getAlgorithms("MessageDigest")`
        // must NOT list them. An alias registered as a service would pass
        // every other assertion in this test.
        for (alias, canonical) in [("SHAKE128", "SHAKE128-256"), ("SHAKE256", "SHAKE256-512")] {
            assert!(
                get_service_entry("SUN", "MessageDigest", alias).is_some(),
                "{alias} must resolve through the alias map to {canonical}"
            );
            assert!(
                !advertised
                    .iter()
                    .any(|(_, a)| a.eq_ignore_ascii_case(alias)),
                "{alias} is an ALIAS and must stay out of Security.getAlgorithms, \
                 which is where HotSpot keeps it"
            );
        }
    }

    /// Every `MessageDigest` alias HotSpot's SUN declares is one this seed
    /// declares too, resolves to a service this VM can compute, and stays out
    /// of `Security.getAlgorithms`.
    ///
    /// The list below is the forty rows of
    /// `Security.getProvider("SUN").keySet()` on HotSpot 25, filtered to
    /// `Alg.Alias.MessageDigest.*` and written out independently of the seed —
    /// which is the whole value of it. Three of the forty were present before
    /// 2026-08-24; `MessageDigest.getInstance("SHA1", "SUN")` was one of the
    /// thirty-seven that were not, and it is a `NoSuchAlgorithmException`
    /// against a provider that has served SHA-1 since the first seed.
    ///
    /// Two directions, both load-bearing:
    ///
    ///   * the alias must RESOLVE — `check_provider_ownership` reads
    ///     `get_service_entry`, so a missing row refuses the call outright;
    ///   * the alias must not be ADVERTISED — registering these as services
    ///     instead would satisfy the first half and make
    ///     `Security.getAlgorithms("MessageDigest")` answer 55 where HotSpot
    ///     answers 15.
    #[test]
    fn sun_message_digest_aliases_all_resolve_to_a_seeded_service() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        let advertised = all_advertised("MessageDigest");
        const HOTSPOT_SUN_MD_ALIASES: &[(&str, &str)] = &[
            ("SHA", "SHA-1"),
            ("SHA1", "SHA-1"),
            ("SHA224", "SHA-224"),
            ("SHA256", "SHA-256"),
            ("SHA384", "SHA-384"),
            ("SHA512", "SHA-512"),
            ("SHA512/224", "SHA-512/224"),
            ("SHA512/256", "SHA-512/256"),
            ("SHAKE128", "SHAKE128-256"),
            ("SHAKE256", "SHAKE256-512"),
            ("1.2.840.113549.2.2", "MD2"),
            ("1.2.840.113549.2.5", "MD5"),
            ("1.3.14.3.2.26", "SHA-1"),
            ("2.16.840.1.101.3.4.2.1", "SHA-256"),
            ("2.16.840.1.101.3.4.2.2", "SHA-384"),
            ("2.16.840.1.101.3.4.2.3", "SHA-512"),
            ("2.16.840.1.101.3.4.2.4", "SHA-224"),
            ("2.16.840.1.101.3.4.2.5", "SHA-512/224"),
            ("2.16.840.1.101.3.4.2.6", "SHA-512/256"),
            ("2.16.840.1.101.3.4.2.7", "SHA3-224"),
            ("2.16.840.1.101.3.4.2.8", "SHA3-256"),
            ("2.16.840.1.101.3.4.2.9", "SHA3-384"),
            ("2.16.840.1.101.3.4.2.10", "SHA3-512"),
            ("2.16.840.1.101.3.4.2.11", "SHAKE128-256"),
            ("2.16.840.1.101.3.4.2.12", "SHAKE256-512"),
            ("OID.1.2.840.113549.2.2", "MD2"),
            ("OID.1.2.840.113549.2.5", "MD5"),
            ("OID.1.3.14.3.2.26", "SHA-1"),
            ("OID.2.16.840.1.101.3.4.2.1", "SHA-256"),
            ("OID.2.16.840.1.101.3.4.2.2", "SHA-384"),
            ("OID.2.16.840.1.101.3.4.2.3", "SHA-512"),
            ("OID.2.16.840.1.101.3.4.2.4", "SHA-224"),
            ("OID.2.16.840.1.101.3.4.2.5", "SHA-512/224"),
            ("OID.2.16.840.1.101.3.4.2.6", "SHA-512/256"),
            ("OID.2.16.840.1.101.3.4.2.7", "SHA3-224"),
            ("OID.2.16.840.1.101.3.4.2.8", "SHA3-256"),
            ("OID.2.16.840.1.101.3.4.2.9", "SHA3-384"),
            ("OID.2.16.840.1.101.3.4.2.10", "SHA3-512"),
            ("OID.2.16.840.1.101.3.4.2.11", "SHAKE128-256"),
            ("OID.2.16.840.1.101.3.4.2.12", "SHAKE256-512"),
        ];
        assert_eq!(
            HOTSPOT_SUN_MD_ALIASES.len(),
            40,
            "the measured HotSpot 25 alias set is forty rows; a loop over a              truncated list passes vacuously"
        );
        for (alias, canonical) in HOTSPOT_SUN_MD_ALIASES {
            let entry = get_service_entry("SUN", "MessageDigest", alias);
            assert!(
                entry.is_some(),
                "SUN answers MessageDigest.{alias} on HotSpot 25 and this seed                  has no row for it, so check_provider_ownership refuses                  getInstance({alias:?}, \"SUN\") outright"
            );
            assert!(
                crate::jca::message_digest::algorithm_supported_public(canonical),
                "{alias} resolves to {canonical}, which the digest engine                  cannot compute: the alias would trade a refusal for a failure                  one call later"
            );
            assert!(
                !advertised
                    .iter()
                    .any(|(_, a)| a.eq_ignore_ascii_case(alias)),
                "{alias} is an ALIAS and must stay out of                  Security.getAlgorithms, which is where HotSpot keeps it"
            );
        }
        assert_eq!(
            advertised
                .iter()
                .filter(|(provider, _)| provider == "SUN")
                .count(),
            15,
            "SUN advertises fifteen MessageDigest services on HotSpot 25;              adding aliases must not move that number"
        );
    }

    /// Every row of [`MEASURED_JDK25_PROVIDER_ALIASES`] resolves, and resolves
    /// to a service its own provider actually registers.
    ///
    /// The table is a transcript of HotSpot 25 filtered by what this VM can
    /// serve, and both halves of that sentence are load-bearing:
    ///
    ///   * a row whose ALIAS does not resolve is dead weight —
    ///     `check_provider_ownership` reads `get_service_entry`, so the row
    ///     exists precisely to make that lookup succeed;
    ///   * a row whose CANONICAL is not a registered service of the SAME
    ///     provider would trade a refusal at `getInstance` for a failure one
    ///     call later, which is the trap the 70 excluded rows are excluded to
    ///     avoid. Seeding those is how an alias table starts lying.
    ///
    /// Also asserts the table did not shrink and that aliases stay out of
    /// `Security.getAlgorithms`, which is where HotSpot keeps them.
    #[test]
    fn every_measured_alias_resolves_to_a_serviceable_canonical() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        // The measurement was taken on a VM with real-JCA routing on, and
        // three seeders are conditional on it (`register` calls them only
        // under `real_jca_mode() || route_ec_to_real() || route_dsa_to_real()`).
        // `SUN AlgorithmParameters DSA` is one of the services they add, and
        // an alias row pointing at it is honest only in the configuration
        // that has it — so seed what the measured VM had, rather than
        // weakening the assertion until the narrower default passes.
        seed_sun_dsa_services();
        seed_sunjsse_services();
        assert_eq!(
            MEASURED_JDK25_PROVIDER_ALIASES.len(),
            264,
            "the measured table is 264 rows; a loop over a truncated list              passes vacuously"
        );
        for (provider, engine, alias, canonical) in MEASURED_JDK25_PROVIDER_ALIASES {
            // The row itself. This is the invariant the seed establishes, and
            // it holds for every engine whether or not that engine registers
            // SERVICES: `KeyAgreement` and `KEM` are served from hand-written
            // SPI tables and register none, so asserting a service entry here
            // would fail on rows that work perfectly.
            let resolved =
                canonical_service_algorithm(Some(provider), engine, alias).unwrap_or_default();
            assert!(
                resolved.eq_ignore_ascii_case(canonical),
                "{provider} declares Alg.Alias.{engine}.{alias} = {canonical} on                  HotSpot 25; this seed resolves it to {resolved:?}"
            );
            // And where the canonical IS a registered service, the alias must
            // reach it through `get_service_entry` too — that is the lookup
            // `check_provider_ownership` performs, and the gate every
            // named-provider `getInstance` passes through.
            if get_service_entry(provider, engine, canonical).is_some() {
                assert!(
                    get_service_entry(provider, engine, alias).is_some(),
                    "{provider}.{engine}.{canonical} is a registered service but                      the alias {alias} does not reach it, so                      check_provider_ownership refuses                      getInstance({alias:?}, {provider:?}) outright"
                );
            }
            let advertised = all_advertised(engine);
            assert!(
                !advertised
                    .iter()
                    .any(|(p, a)| p == provider && a.eq_ignore_ascii_case(alias)),
                "{provider}.{engine}.{alias} is an ALIAS and must stay out of                  Security.getAlgorithms"
            );
        }
    }

    /// A seeded provider's property view is its registry, in the provider's
    /// own spelling.
    ///
    /// `keySet` answered EMPTY for every JDK provider until 2026-08-27 —
    /// `put_service` / `put_alias` captured into the structured maps and
    /// nothing mirrored them into the property table `keySet`, `entrySet`,
    /// `values`, `keys`, `elements`, `size`, `isEmpty` and `getProperty` all
    /// read. Measured against HotSpot 25, which answers 257 keys on SUN.
    ///
    /// The SPELLING half is the part a test is needed for. Lookups normalise
    /// to upper case, and rendering a row from the normalised key would
    /// produce `ALG.ALIAS.MESSAGEDIGEST.SHA1`, which is not a key any caller
    /// recognises and not one `getProperty` could match. That is why
    /// `AliasEntry` keeps the raw spellings at all, and it is what the exact
    /// string comparisons below pin.
    #[test]
    fn a_seeded_provider_renders_its_registry_as_property_rows() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        let rows = projected_registry_rows("SUN");
        assert!(
            rows.len() >= 100,
            "SUN projects {} rows; HotSpot 25 answers 257 and an empty view is \
             the defect this closes",
            rows.len()
        );
        let find = |key: &str| -> Option<String> {
            rows.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        };
        // A SERVICE row: `<Type>.<Algorithm>` in the provider's spelling.
        let digest = find("MessageDigest.SHA-256")
            .expect("SUN must project MessageDigest.SHA-256 as a property row");
        assert!(
            !digest.trim().is_empty(),
            "a service row's value is its implementation class, never empty"
        );
        // An ALIAS row, and the value is the canonical it names.
        assert_eq!(
            find("Alg.Alias.MessageDigest.SHA1").as_deref(),
            Some("SHA-1"),
            "SUN must project Alg.Alias.MessageDigest.SHA1 = SHA-1, in that \
             spelling: an upper-cased key matches nothing a caller asks for"
        );
        // The upper-cased forms must NOT appear — that is what rendering
        // from the normalised lookup key would have produced.
        for mangled in ["ALG.ALIAS.MESSAGEDIGEST.SHA1", "MESSAGEDIGEST.SHA-256"] {
            assert!(
                find(mangled).is_none(),
                "{mangled} is a normalised LOOKUP key, not a property key"
            );
        }
        // An alias-only service artefact carries no class and is not a row.
        assert!(
            rows.iter().all(|(_, v)| !v.trim().is_empty()),
            "a property row with an empty value is not one any provider advertises"
        );
    }

    /// W7-63 ratchet: no advertised `Signature` name may be refused at
    /// `getInstance`.
    ///
    /// Only this direction is asserted, and the asymmetry is the point.
    /// `Signature.getInstance` used to accept EVERY string and defer the
    /// failure to `sign()`/`verify()` as a `SignatureException`, so a caller
    /// probing with `catch (NoSuchAlgorithmException)` took the wrong branch.
    /// The gate that closed it (`signature::signature_name_is_offered`) is a
    /// disjunction — a name this engine has an index for, OR a name some
    /// provider advertises — so it can only fail in one direction, and this is
    /// that direction.
    ///
    /// It deliberately does NOT assert that every advertised name can compute.
    /// Nine `SunRsaSign` names (`MD2withRSA`, `SHA3-*withRSA`,
    /// `SHA512/224withRSA` and friends) have no `sign_dispatch` arm and fail
    /// at `sign()` with a checked, catchable `SignatureException`. That is an
    /// ordinary unimplemented-algorithm gap, not this record's species: the
    /// name is real, the advertisement is truthful, and the failure is closed.
    /// Asserting serviceability here would red the test for a gap W7-63 did
    /// not open and does not close.
    #[test]
    fn every_advertised_signature_name_is_offered_by_get_instance() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        let advertised = all_advertised("Signature");
        assert!(
            advertised.len() >= 20,
            "the Signature seed looks empty ({} rows): a loop over nothing \
             passes vacuously",
            advertised.len()
        );
        for (provider, algorithm) in &advertised {
            assert!(
                crate::jca::signature::get_instance_offers(algorithm),
                "{provider} advertises Signature.{algorithm}, but \
                 Signature.getInstance refuses it"
            );
        }
        // The anti-vacuity half: prove the gate can say NO. Without this the
        // loop above is satisfied by a predicate that returns `true`
        // unconditionally, which is exactly what `sig_get_instance` used to do.
        for bogus in ["NO-SUCH-SIG", "ML-KEM", "AES", "HmacSHA256", ""] {
            assert!(
                !crate::jca::signature::get_instance_offers(bogus),
                "Signature.getInstance must refuse {bogus:?} — HotSpot 25 raises \
                 NoSuchAlgorithmException for it, measured"
            );
        }
    }

    /// The `KeyPairGenerator` ratchet, in BOTH directions — which is what makes
    /// it different from its `Signature` / `KeyFactory` siblings.
    ///
    /// The one-way form (advertised ⇒ serviceable) cannot see the residual this
    /// closes. `Security.getProviders("KeyPairGenerator.Ed25519")` answered
    /// `<none>` for an algorithm `getInstance("Ed25519")` served, because
    /// `security_get_providers_filtered` reads the service registry and the
    /// built-in algorithms were in NO registry — under-advertised, not
    /// over-advertised, and therefore invisible to a one-way check. A caller
    /// that picks its provider by filter concluded the algorithm did not exist.
    ///
    /// So both halves are asserted: nothing advertised is refused, and nothing
    /// served is unadvertised.
    #[test]
    fn every_kpg_algorithm_this_vm_serves_is_advertised() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        let advertised = all_advertised("KeyPairGenerator");
        assert!(
            advertised.len() >= 15,
            "the KeyPairGenerator seed looks empty ({} rows): a loop over \
             nothing passes vacuously",
            advertised.len()
        );
        for (provider, algorithm) in &advertised {
            assert!(
                crate::jca::key_factory::kpg_get_instance_offers(algorithm),
                "{provider} advertises KeyPairGenerator.{algorithm}, but \
                 KeyPairGenerator.getInstance refuses it"
            );
        }
        // The direction the filter needs, and the one the residual was in.
        // `DH` and `ECDSA` are absent by design: HotSpot registers `DH` as an
        // ALIAS of `DiffieHellman` (aliases resolve but are not advertised),
        // and registers no `ECDSA` generator at all.
        for algorithm in [
            "RSA",
            "RSASSA-PSS",
            "EC",
            "DSA",
            "Ed25519",
            "Ed448",
            "EdDSA",
            "X25519",
            "X448",
            "XDH",
            "ML-DSA",
            "ML-DSA-44",
            "ML-KEM",
            "ML-KEM-512",
            "DiffieHellman",
        ] {
            assert!(
                crate::jca::key_factory::kpg_get_instance_offers(algorithm),
                "{algorithm} must be serviceable"
            );
            assert!(
                find_service_provider("KeyPairGenerator", algorithm).is_some(),
                "KeyPairGenerator.{algorithm} is served but advertised by no \
                 provider — Security.getProviders(\"KeyPairGenerator.{algorithm}\") \
                 answers <none> and a caller that picks its provider by filter \
                 cannot find it"
            );
        }
        // The alias resolves without being advertised, exactly as on HotSpot.
        assert!(
            find_service_provider("KeyPairGenerator", "DH").is_some(),
            "the DH alias must resolve to the DiffieHellman service"
        );
        // Anti-vacuity: prove the registry can say NO.
        for bogus in ["NO-SUCH-KPG", "SLH-DSA", "AES", ""] {
            assert!(
                find_service_provider("KeyPairGenerator", bogus).is_none(),
                "no provider may advertise KeyPairGenerator.{bogus:?}"
            );
        }
    }

    /// W7-63 ratchet: every `KeyFactory` name the seed advertises must be one
    /// `KeyFactory.getInstance` will serve.
    ///
    /// The `SUN` seed advertised the `ML-DSA` UMBRELLA name while
    /// `key_factory::kf_algo_idx` had arms only for `ML-DSA-44/65/87` and
    /// `kf_get_instance` threw on the negative index — advertised and refused.
    /// `signature::algo_idx` carries the umbrella arm, so the two engines
    /// disagreed about one name. The umbrella is now absent from the
    /// `KeyFactory` seed and still present in the `Signature` seed, which
    /// makes each engine's advertisement match ITS OWN implementation rather
    /// than making the two engines agree with each other — that is the
    /// correct invariant, and it is what this asserts.
    #[test]
    fn every_advertised_key_factory_name_is_serviceable() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        let advertised = all_advertised("KeyFactory");
        assert!(
            advertised.len() >= 12,
            "the KeyFactory seed looks empty ({} rows): a loop over nothing \
             passes vacuously",
            advertised.len()
        );
        for (provider, algorithm) in &advertised {
            assert!(
                crate::jca::key_factory::get_instance_offers(algorithm),
                "{provider} advertises KeyFactory.{algorithm}, but \
                 KeyFactory.getInstance refuses it"
            );
        }
        // Both umbrellas WERE stop-advertising fixes and are now
        // implementations: `kf_algo_idx` carries `ALGO_MLDSA_GENERIC` /
        // `ALGO_MLKEM_GENERIC` and `pqc_umbrella_keyfactory_class` drives the
        // JDK's own non-nested factory for each. The rule the earlier
        // assertion encoded has not changed — "either implement the umbrella
        // arm or leave the name out, but never both" — only which side of it
        // these two names sit on. The loop above is what enforces it
        // generically; this is the named pin for the pair that drifted.
        for (provider, umbrella) in [("SUN", "ML-DSA"), ("SunJCE", "ML-KEM")] {
            assert!(
                get_service_entry(provider, "KeyFactory", umbrella).is_some(),
                "{provider} implements the {umbrella} KeyFactory umbrella and must \
                 advertise it — an implemented-but-unadvertised name is invisible to \
                 Security.getProviders(filter), which is how a caller picks a provider"
            );
            assert!(
                crate::jca::key_factory::get_instance_offers(umbrella),
                "{umbrella} is advertised for KeyFactory and must be serviceable"
            );
        }
        assert!(
            get_service_entry("SUN", "Signature", "ML-DSA").is_some(),
            "Signature DOES implement the ML-DSA umbrella (signature::algo_idx \
             carries SIG_MLDSA) and must keep advertising it — the two engines are \
             each truthful about THEMSELVES, which is the invariant, not that they \
             agree with each other"
        );
        for param_set in ["ML-DSA-44", "ML-DSA-65", "ML-DSA-87"] {
            assert!(
                get_service_entry("SUN", "KeyFactory", param_set).is_some(),
                "{param_set} is serviceable and must stay advertised"
            );
        }
        for param_set in ["ML-KEM-512", "ML-KEM-768", "ML-KEM-1024"] {
            assert!(
                get_service_entry("SunJCE", "KeyFactory", param_set).is_some(),
                "{param_set} is serviceable and must stay advertised"
            );
        }
    }

    /// The same ratchet for `Mac`, in the shape
    /// `every_advertised_sunjce_cipher_is_serviceable` established — and for the
    /// same reason: `Mac` is where the wrong-algorithm defect was WORST, because
    /// a MAC that verifies is itself the security decision, and two peers
    /// computing the same wrong MAC interoperate happily.
    ///
    /// This reads the advertised set out of the registry rather than restating
    /// it, so the list cannot be updated on one side only. `HmacSHA224` was
    /// added to both sides on 2026-08-12; a census written by hand would have
    /// been the thing that went stale.
    #[test]
    fn every_advertised_sunjce_mac_is_computable() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        // The SECOND seeder, added 2026-09-02, and the reason this line is
        // here: the population below is read out of the registry, but the
        // registry holds only what the seeders this test CALLS have put in it.
        // Sixteen new `Mac` services seeded from a function this test did not
        // call left the ratchet passing over a set that no longer matched the
        // VM's — the same "population transcribed from the registrar rather
        // than from the platform" shape `every_public_mac_method_is_registered`
        // was caught by (E25-R11).
        seed_sunjce_pbe_mac_services();
        let advertised: Vec<String> = services()
            .lock()
            .get("SunJCE")
            .expect("the SunJCE seed must have run")
            .values()
            .filter(|e| e.type_str == "Mac")
            .map(|e| e.algorithm.clone())
            .collect();
        assert!(
            advertised.len() >= 28,
            "the SunJCE Mac seed looks empty: {advertised:?}"
        );
        for algorithm in &advertised {
            let bytes = crate::phases_late::ssl_security::mac_compute_hmac(algorithm, b"k", b"d");
            // SERVICEABLE, which is a disjunction — and was a single term until
            // the PKCS#12 / PBMAC1 / SSL MAC families landed.
            //
            // None of those sixteen is an HMAC this crate computes (PKCS#12
            // v1.0 B.2 derivation, PBKDF2-then-HMAC, and the SSL 3.0 pad-byte
            // construction respectively), and none needs to be:
            // `Mac.getInstance` falls to `build_real_mac` for a name it cannot
            // compute, so the platform's own class serves them. The W4-3
            // property this test exists for is "advertised implies
            // serviceable", not "advertised implies computed HERE" — but the
            // second arm has to be a REAL class, because a `.Native` marker row
            // is precisely an advertisement with nothing behind it.
            let routed = get_service_entry("SunJCE", "Mac", algorithm)
                .map(|e| {
                    let c = e.class_name.trim().to_string();
                    !c.is_empty() && !c.ends_with(".Native")
                })
                .unwrap_or(false);
            assert!(
                bytes.is_some() || routed,
                "SunJCE advertises Mac.{algorithm}, and neither mac_compute_hmac nor a real                  implementation class serves it"
            );
            if bytes.is_none() {
                continue;
            }
            let len = crate::phases_late::ssl_security::mac_output_length(algorithm);
            assert!(
                bytes.is_some(),
                "SunJCE advertises Mac.{algorithm}, but mac_compute_hmac produces nothing — \
                 advertising a MAC the engine cannot compute is the W4-3 defect"
            );
            assert_eq!(
                len,
                bytes.as_ref().map(Vec::len),
                "Mac.{algorithm}: getMacLength() and the bytes doFinal returns must agree — \
                 the retired `_ => 32` arm made them agree with each other and with nothing else"
            );
        }
        // The reverse direction, which is the one a census does not enumerate:
        // implemented but unadvertised. Every name `mac_compute_hmac` answers
        // must have a service row, or `Security.getAlgorithms("Mac")` under-
        // reports what `Mac.getInstance` will actually serve.
        for implemented in [
            "HmacMD5",
            "HmacSHA1",
            "HmacSHA224",
            "HmacSHA256",
            "HmacSHA384",
            "HmacSHA512",
        ] {
            assert!(
                get_service_entry("SunJCE", "Mac", implemented).is_some(),
                "{implemented} is computed by mac_compute_hmac but not advertised: the two \
                 lists have drifted, which is the defect this test exists for"
            );
        }
    }

    /// And for `KeyGenerator`, which is where the drift was widest: the seed
    /// carried three names while `phases_early::keygen_default_bits` implemented
    /// fourteen, so `KeyGenerator.getInstance("Blowfish")` answered
    /// `NoSuchAlgorithmException: Blowfish KeyGenerator not available` —
    /// measured on this tree's own binary — for an algorithm BOTH modes could
    /// serve.
    ///
    /// The check runs in the implemented→advertised direction only, on purpose.
    /// The other direction is not this test's to make: in `--real-jdk` mode
    /// `KeyGenerator.getInstance` resolves through this registry and then
    /// INSTANTIATES the named class out of the real image, so an advertised row
    /// is serviceable if and only if that class loads — which no unit test in
    /// this crate can observe. `keygen_default_bits` is the `--synthetic-jdk`
    /// half, and it is the half a Rust test can hold to account.
    #[test]
    fn every_keygenerator_the_engine_implements_is_advertised() {
        let _lock = reset_service_state_for_tests();
        seed_direct_native_engine_services();
        for implemented in [
            "AES",
            "ARCFOUR",
            "Blowfish",
            "ChaCha20",
            "DES",
            "DESede",
            "HmacMD5",
            "HmacSHA1",
            "HmacSHA224",
            "HmacSHA256",
            "HmacSHA384",
            "HmacSHA512",
            "RC2",
        ] {
            assert!(
                get_service_entry("SunJCE", "KeyGenerator", implemented).is_some(),
                "phases_early::keygen_default_bits generates a {implemented} key, but SunJCE \
                 does not advertise it — implemented-but-unadvertised is the quiet half of \
                 this defect, because nothing asks for a name nobody publishes"
            );
        }
        // `RC4` is the alias, `ARCFOUR` the service, exactly as on SunJCE.
        assert!(get_service_entry("SunJCE", "KeyGenerator", "RC4").is_some());
        // THE BICONDITIONAL, over HotSpot's own list, and derived rather than
        // restated.
        //
        // This was two hand-written lists — implemented-and-advertised above,
        // and a literal `["HmacSHA3-256", "HmacSHA512/256", "SunTlsPrf"]` of
        // names that must NOT be advertised. The second went stale the moment
        // `keygen_default_bits` grew arms for the first two (2026-09-02), and
        // it failed saying they "must not be advertised" — which by then was
        // false. A list that states the answer cannot check it.
        //
        // So: for every `KeyGenerator` name SunJCE registers on HotSpot 25,
        // advertised HERE if and only if this engine can generate it. Both
        // directions are the defect: advertising a name the synthetic path
        // refuses, and refusing to publish a name it serves.
        const HOTSPOT_SUNJCE_KEYGENERATORS: &[&str] = &[
            "AES",
            "ARCFOUR",
            "Blowfish",
            "ChaCha20",
            "DES",
            "DESede",
            "HmacMD5",
            "HmacSHA1",
            "HmacSHA224",
            "HmacSHA256",
            "HmacSHA384",
            "HmacSHA512",
            "HmacSHA512/224",
            "HmacSHA512/256",
            "HmacSHA3-224",
            "HmacSHA3-256",
            "HmacSHA3-384",
            "HmacSHA3-512",
            "RC2",
            "SunTls12Prf",
            "SunTlsKeyMaterial",
            "SunTlsMasterSecret",
            "SunTlsPrf",
            "SunTlsRsaPremasterSecret",
        ];
        // ADVERTISED iff SERVICEABLE, and "serviceable" is now a disjunction:
        // this crate generates the key itself (`keygen_default_bits`), or the
        // row names a REAL platform class that `build_real_key_generator`
        // instantiates on the engine's refusal. Before 2026-09-02 only the
        // first arm existed and the five `SunTls*` names were asserted ABSENT;
        // widening the predicate rather than deleting the assertion is what
        // keeps this a check instead of a restatement.
        for name in HOTSPOT_SUNJCE_KEYGENERATORS {
            let generates = crate::phases_early::keygen_default_bits(name).is_some();
            let delegates = service_implementation_class("KeyGenerator", "SunJCE", name).is_some();
            let advertised = get_service_entry("SunJCE", "KeyGenerator", name).is_some();
            assert_eq!(
                generates || delegates,
                advertised,
                "KeyGenerator.{name}: generates={generates} delegates={delegates} but SunJCE                  advertises={advertised} — advertised and serviceable must agree in BOTH                  directions"
            );
        }
        // The five `SunTls*` KDFs take `TlsKeyMaterialParameterSpec`-family
        // specs, so this crate must NOT claim to generate them from a key size
        // — they are served by routing to the platform's own generator, and a
        // `keygen_default_bits` arm appearing here would mean someone had
        // fabricated a key where a KDF belongs. The disjunction above is what
        // admits them; this pins WHICH arm may do it.
        for tls in [
            "SunTlsPrf",
            "SunTls12Prf",
            "SunTlsMasterSecret",
            "SunTlsKeyMaterial",
            "SunTlsRsaPremasterSecret",
        ] {
            assert!(
                crate::phases_early::keygen_default_bits(tls).is_none(),
                "{tls} is a parameter-spec-driven KDF, not a key size"
            );
            let class = service_implementation_class("KeyGenerator", "SunJCE", tls)
                .unwrap_or_else(|| panic!("{tls} must be advertised with a real class"));
            assert!(
                class.starts_with("com.sun.crypto.provider.Tls") && !class.ends_with(".Native"),
                "{tls} must route to the platform generator, got {class:?}"
            );
        }
    }

    /// W3-7. Every name the platform JDK 25 SunJSSE provider registers must be
    /// accepted, in the spellings a caller actually writes. `RJdkSecurity.tls`
    /// only probes the negative half; the positive half is what breaks every
    /// HTTPS suite if an accept list is drawn too narrow, so pin it here.
    #[test]
    fn ssl_context_protocol_supported_accepts_every_jdk25_name() {
        for p in [
            "TLS", "tls", "TLSv1", "TLSv1.1", "TLSv1.2", "tlsv1.2", "TLSV1.2", "TLSv1.3", "SSL",
            "ssl", "SSLv3", "SSLV3", "Default", "default", "DEFAULT", "DTLS", "dtls", "DTLSv1.0",
            "DTLSv1.2",
        ] {
            assert!(
                ssl_context_protocol_supported(p),
                "{p:?} is a real JDK 25 SSLContext protocol and must not be refused"
            );
        }
    }

    /// MUST RAISE. Every one of these was probed on jdk-25.0.3.9-hotspot and
    /// answered `NoSuchAlgorithmException: <name> SSLContext not available`.
    /// The last four pin the normalisation: JCA lookup folds case and NOTHING
    /// else, so a punctuation-stripping or space-trimming accept would
    /// fabricate a context HotSpot refuses.
    #[test]
    fn ssl_context_protocol_supported_rejects_names_hotspot_rejects() {
        let _lock = reset_service_state_for_tests();
        for p in [
            "NO-SUCH-TLS",
            "SSLv2",
            "TLSv1.4",
            "NoSuchThing",
            "",
            "TLSv12",
            "T-L-S",
            "TLS ",
            " TLS",
        ] {
            assert!(
                !ssl_context_protocol_supported(p),
                "{p:?} is not a JDK SSLContext protocol and must be refused"
            );
        }
    }

    /// A protocol a caller's own provider registered must resolve even though
    /// it is absent from the built-in list — the second accept ground.
    #[test]
    fn ssl_context_protocol_supported_honours_a_caller_registered_service() {
        let _lock = reset_service_state_for_tests();
        assert!(!ssl_context_protocol_supported("Conscrypt-TLS"));
        apply_legacy_put(
            "SUN",
            "SSLContext.Conscrypt-TLS",
            "org.conscrypt.OpenSSLContextImpl",
        );
        assert!(ssl_context_protocol_supported("Conscrypt-TLS"));
    }

    #[test]
    fn sunjsse_seed_registers_the_full_jdk25_sslcontext_table() {
        let _lock = reset_service_state_for_tests();
        seed_sunjsse_services();
        for algo in [
            "TLS", "TLSv1", "TLSv1.1", "TLSv1.2", "TLSv1.3", "Default", "DTLS", "DTLSv1.0",
            "DTLSv1.2",
        ] {
            assert!(
                get_service_entry("SunJSSE", "SSLContext", algo).is_some(),
                "SunJSSE must service SSLContext.{algo}"
            );
        }
        // Aliases, with the platform's own targets.
        assert_eq!(
            get_service_entry("SunJSSE", "SSLContext", "SSL")
                .unwrap()
                .algorithm,
            "TLS"
        );
        assert_eq!(
            get_service_entry("SunJSSE", "SSLContext", "SSLv3")
                .unwrap()
                .algorithm,
            "TLSv1"
        );
        // And the negative half: no fabricated entry for a bogus name.
        assert!(get_service_entry("SunJSSE", "SSLContext", "NO-SUCH-TLS").is_none());
    }

    #[test]
    fn alias_resolves_to_canonical_service() {
        let _lock = reset_service_state_for_tests();
        // Register the canonical service plus an alias pointing to it.
        apply_legacy_put("BC", "MessageDigest.SHA-256", "org.bc.SHA256");
        apply_legacy_put("BC", "Alg.Alias.MessageDigest.SHA256", "SHA-256");
        // Lookup by alias (no dash) must resolve to the canonical entry.
        let via_alias = get_service_entry("BC", "MessageDigest", "SHA256").unwrap();
        assert_eq!(via_alias.algorithm, "SHA-256");
        assert_eq!(via_alias.class_name, "org.bc.SHA256");
        // Direct canonical lookup still works.
        let direct = get_service_entry("BC", "MessageDigest", "SHA-256").unwrap();
        assert_eq!(direct.class_name, "org.bc.SHA256");
    }

    #[test]
    fn put_native_populates_service_via_string_keys() {
        let _lock = reset_service_state_for_tests();
        let mut ctx = MockNativeContext::new();
        // Build a Provider receiver with `name="BC"` populated.
        let prov = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        let name = ctx.create_string("BC");
        ctx.set_field(prov, 0, Value::Object(Some(name)));
        ctx.set_field(prov, 1, Value::Double(1.80));

        let key = ctx.create_string("Cipher.AES/GCM/NoPadding");
        let value = ctx.create_string("org.bouncycastle.jcajce.provider.symmetric.AES$GCM");
        let args = [
            Value::Object(Some(prov)),
            Value::Object(Some(key)),
            Value::Object(Some(value)),
        ];
        let res = provider_put_native(&mut ctx, &args);
        assert!(res.is_ok());
        // Hashtable.put returns previous value — null for first put.
        assert!(matches!(res.unwrap(), Some(Value::Object(None))));

        // The service map must now have the entry under "BC".
        let entry = get_service_entry("BC", "Cipher", "AES/GCM/NoPadding").unwrap();
        assert_eq!(
            entry.class_name,
            "org.bouncycastle.jcajce.provider.symmetric.AES$GCM"
        );
    }

    #[test]
    fn parse_legacy_put_native_alias_round_trip() {
        let _lock = reset_service_state_for_tests();
        let mut ctx = MockNativeContext::new();
        let prov = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        let name = ctx.create_string("BC");
        ctx.set_field(prov, 0, Value::Object(Some(name)));
        ctx.set_field(prov, 1, Value::Double(1.80));

        // First, the canonical service.
        let key1 = ctx.create_string("MessageDigest.SHA-256");
        let val1 = ctx.create_string("org.bc.SHA256");
        let _ = provider_parse_legacy_put_native(
            &mut ctx,
            &[
                Value::Object(Some(prov)),
                Value::Object(Some(key1)),
                Value::Object(Some(val1)),
            ],
        );
        // Then, an alias.
        let key2 = ctx.create_string("Alg.Alias.MessageDigest.SHA256");
        let val2 = ctx.create_string("SHA-256");
        let _ = provider_parse_legacy_put_native(
            &mut ctx,
            &[
                Value::Object(Some(prov)),
                Value::Object(Some(key2)),
                Value::Object(Some(val2)),
            ],
        );

        // Resolve via alias.
        let resolved = get_service_entry("BC", "MessageDigest", "SHA256").unwrap();
        assert_eq!(resolved.algorithm, "SHA-256");
    }

    #[test]
    fn get_service_native_returns_null_for_unknown_algo() {
        let _lock = reset_service_state_for_tests();
        let mut ctx = MockNativeContext::new();
        let prov = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        let name = ctx.create_string("BC");
        ctx.set_field(prov, 0, Value::Object(Some(name)));
        ctx.set_field(prov, 1, Value::Double(1.80));

        let type_s = ctx.create_string("Cipher");
        let algo_s = ctx.create_string("MissingAlgo");
        let res = provider_get_service_native(
            &mut ctx,
            &[
                Value::Object(Some(prov)),
                Value::Object(Some(type_s)),
                Value::Object(Some(algo_s)),
            ],
        );
        assert!(res.is_ok());
        // Null = NoSuchAlgorithmException at the JDK call site.
        assert!(matches!(res.unwrap(), Some(Value::Object(None))));
    }

    #[test]
    fn provider_service_new_instance_recovers_class_name_from_registry() {
        let _lock = reset_service_state_for_tests();
        apply_legacy_put(
            "WildFlyElytron",
            "SaslServerFactory.JBOSS-LOCAL-USER",
            "com.example.NoSuchSaslServerFactory",
        );

        let mut ctx = MockNativeContext::new();
        let provider = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        let provider_name = ctx.create_string("WildFlyElytron");
        ctx.set_field(provider, 0, Value::Object(Some(provider_name)));
        ctx.set_field(provider, 1, Value::Double(1.0));

        let service = alloc_service(&mut ctx);
        let svc_type = ctx.create_string("SaslServerFactory");
        let algorithm = ctx.create_string("JBOSS-LOCAL-USER");
        ctx.set_field(service, 0, Value::Object(Some(svc_type)));
        ctx.set_field(service, 1, Value::Object(Some(algorithm)));
        ctx.set_field(service, 2, Value::Object(Some(provider)));
        ctx.set_field(service, 3, Value::Object(None));

        let recovered = provider_service_get_class_name(&mut ctx, &[Value::Object(Some(service))])
            .expect("getClassName should not throw");
        let recovered = match recovered {
            Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
            other => panic!("expected recovered className string, got {other:?}"),
        };
        assert_eq!(recovered, "com.example.NoSuchSaslServerFactory");

        let instantiated = provider_service_new_instance(
            &mut ctx,
            &[Value::Object(Some(service)), Value::Object(None)],
        )
        .expect("registry-recovered className should reach instantiation in the mock VM");
        assert!(
            matches!(instantiated, Some(Value::Object(Some(_)))),
            "expected mock VM allocation after registry fallback, got {instantiated:?}"
        );
    }

    #[test]
    fn provider_put_service_native_uses_slot_fallback_and_remembers_class_name() {
        let _lock = reset_service_state_for_tests();
        let mut ctx = MockNativeContext::new();
        let provider = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        let provider_name = ctx.create_string("WildFlyElytron");
        ctx.set_field(provider, 0, Value::Object(Some(provider_name)));
        ctx.set_field(provider, 1, Value::Double(1.0));

        let service = alloc_service(&mut ctx);
        let svc_type = ctx.create_string("SaslServerFactory");
        let algorithm = ctx.create_string("JBOSS-LOCAL-USER");
        let class_name =
            ctx.create_string("org.wildfly.security.sasl.localuser.LocalUserServerFactory");
        ctx.set_field(service, 0, Value::Object(Some(svc_type)));
        ctx.set_field(service, 1, Value::Object(Some(algorithm)));
        ctx.set_field(service, 2, Value::Object(Some(provider)));
        ctx.set_field(service, 3, Value::Object(Some(class_name)));

        provider_put_service_native(
            &mut ctx,
            &[Value::Object(Some(provider)), Value::Object(Some(service))],
        )
        .expect("putService native should not throw");

        let entry = get_service_entry("WildFlyElytron", "SaslServerFactory", "JBOSS-LOCAL-USER")
            .expect("putService should populate the provider service registry");
        assert_eq!(
            entry.class_name,
            "org.wildfly.security.sasl.localuser.LocalUserServerFactory"
        );
        let ih = ctx.identity_hash_code(service) as i64;
        assert_eq!(
            service_classname_table()
                .lock()
                .get(&ih)
                .cloned()
                .as_deref(),
            Some("org.wildfly.security.sasl.localuser.LocalUserServerFactory")
        );
    }

    #[test]
    fn get_service_native_returns_populated_service_for_registered_algo() {
        let _lock = reset_service_state_for_tests();
        // Pre-populate the service map for "BC".
        apply_legacy_put(
            "BC",
            "Cipher.AES/GCM/NoPadding",
            "org.bouncycastle.jcajce.provider.symmetric.AES$GCM",
        );
        let mut ctx = MockNativeContext::new();
        let prov = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        let name = ctx.create_string("BC");
        ctx.set_field(prov, 0, Value::Object(Some(name)));
        ctx.set_field(prov, 1, Value::Double(1.80));

        let type_s = ctx.create_string("Cipher");
        let algo_s = ctx.create_string("AES/GCM/NoPadding");
        let res = provider_get_service_native(
            &mut ctx,
            &[
                Value::Object(Some(prov)),
                Value::Object(Some(type_s)),
                Value::Object(Some(algo_s)),
            ],
        );
        let svc = match res.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected non-null Service object, got {other:?}"),
        };
        // Synthetic slot 0 = type, slot 1 = algorithm, slot 3 = className.
        let svc_type = match ctx.get_field(svc, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let svc_algo = match ctx.get_field(svc, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let svc_class = match ctx.get_field(svc, 3) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        assert_eq!(svc_type, "Cipher");
        assert_eq!(svc_algo, "AES/GCM/NoPadding");
        assert_eq!(
            svc_class,
            "org.bouncycastle.jcajce.provider.symmetric.AES$GCM"
        );
    }

    #[test]
    fn put_get_service_natives_registered_via_register_fn() {
        // The natives must be exposed via `register()` so the WP6.5
        // wiring in `jca::register_jca_natives` picks them up.
        let mut r = NativeMethodRegistry::new();
        register(&mut r);

        assert!(
            r.find(
                "java/security/Provider",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some(),
            "Provider.put must be registered for BouncyCastle's addAlgorithm chain"
        );
        assert!(
            r.find(
                "java/security/Provider",
                "parseLegacyPut",
                "(Ljava/lang/String;Ljava/lang/String;)V"
            )
            .is_some(),
            "Provider.parseLegacyPut must be registered for BC's older-API path"
        );
        assert!(
            r.find(
                "java/security/Provider",
                "getService",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;"
            )
            .is_some(),
            "Provider.getService must be registered for Cipher.getInstance lookups"
        );
        assert!(
            r.find(
                "java/security/Provider$Service",
                "getClassName",
                "()Ljava/lang/String;"
            )
            .is_some(),
            "Provider$Service.getClassName must be registered for SPI instantiation"
        );
    }
}
/// Provider-qualified KeyStore construction must produce a real Java wrapper
/// with a real provider SPI; Elytron calls this overload for `applicationKS`.
fn key_store_get_instance_with_provider(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let type_ref = obj_arg(args, 0)?;
    let provider_ref = obj_arg(args, 1)?;
    let type_name = ctx.read_string(type_ref).unwrap_or_default();
    let provider_name = read_provider_name_version(ctx, provider_ref)
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty())
        .or_else(|| find_service_provider("KeyStore", &type_name))
        .unwrap_or_default();

    let type_pin = ctx.pin_native_root(type_ref);
    let provider_pin = ctx.pin_native_root(provider_ref);
    let result = (|| {
        let spi = match build_jca_impl(ctx, &provider_name, "KeyStore", &type_name) {
            Some(Ok(Some(Value::Object(Some(spi))))) => spi,
            Some(Err(err)) => return Err(err),
            _ => {
                return Err(throw_no_such_algorithm(
                    ctx,
                    &format!("no KeyStore {type_name} implementation for provider {provider_name}"),
                ))
            }
        };
        let type_ref = ctx.read_native_pin(type_pin, type_ref);
        let provider_ref = ctx.read_native_pin(provider_pin, provider_ref);
        let provider_ref = if provider_name.is_empty() {
            resolve_or_make_provider(ctx, "SUN")
        } else {
            Ok(provider_ref)
        }?;
        ctx.new_object_initialized(
            "java/security/KeyStore",
            "(Ljava/security/KeyStoreSpi;Ljava/security/Provider;Ljava/lang/String;)V",
            &[
                Value::Object(Some(spi)),
                Value::Object(Some(provider_ref)),
                Value::Object(Some(type_ref)),
            ],
        )
    })();
    ctx.unpin_native_roots(provider_pin);
    ctx.unpin_native_roots(type_pin);
    result
}
