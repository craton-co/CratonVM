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
//! Allocated through `alloc_concurrent_synthetic("java/security/Provider", N)`,
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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, VmError};
use cratonvm_types::{ObjectRef, Value};

use rustc_hash::FxHashMap;

use crate::{alloc_concurrent_synthetic, obj_arg};

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
    "coverage: Cipher{AES (ECB/CBC/GCM, PKCS5Padding/NoPadding)} via in-tree AES/GCM \
     (see jca/cipher.rs); KeyFactory{EC}, Signature{SHA256withECDSA,SHA384withECDSA,Ed25519}";
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
        parking_lot::Mutex::new(vec![
            ("SUN".to_string(), 25.0, COVERAGE_SUN),
            ("SunRsaSign".to_string(), 25.0, COVERAGE_SUN_RSA_SIGN),
            // SunEC: the EC algorithm wiring lives in jca/signature.rs
            // (ECDSA Signature, EC KeyFactory) but BouncyCastle's EC
            // <clinit> shim above no-ops EC mapping under WildFly; mark
            // as unbacked at the provider level since
            // `getService("KeyPairGenerator","EC")` on the SunEC Provider
            // returns null (entries land under "BC" only).
            ("SunEC".to_string(), 25.0, COVERAGE_UNBACKED),
            ("SunJSSE".to_string(), 25.0, COVERAGE_UNBACKED),
            ("SunJCE".to_string(), 25.0, COVERAGE_SUN_JCE),
            ("SunJGSS".to_string(), 25.0, COVERAGE_UNBACKED),
            ("SunSASL".to_string(), 25.0, COVERAGE_UNBACKED),
            ("XMLDSig".to_string(), 25.0, COVERAGE_UNBACKED),
            ("SunPCSC".to_string(), 25.0, COVERAGE_UNBACKED),
            ("JdkLDAP".to_string(), 25.0, COVERAGE_UNBACKED),
            ("JdkSASL".to_string(), 25.0, COVERAGE_UNBACKED),
            ("SunMSCAPI".to_string(), 25.0, COVERAGE_UNBACKED),
            ("SunPKCS11".to_string(), 25.0, COVERAGE_UNBACKED),
        ])
    })
}

fn snapshot() -> Vec<(String, f64, &'static str)> {
    provider_chain().lock().clone()
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

/// Materialise a fresh `java.security.Provider` synthetic with name +
/// numeric version. Used by every read-side path
/// (`getProviders`, `getProvider`); we never cache `ObjectRef` values
/// across callbacks so the heap is free to GC the previous instance.
pub(crate) fn make_provider(
    ctx: &mut dyn NativeContext,
    name: &str,
    version: f64,
    coverage: &str,
) -> ObjectRef {
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
    let p = alloc_concurrent_synthetic(ctx, "java/security/Provider", 8);
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

    // Synthetic fallback — populate the legacy slots 0/1/2 too so
    // `phases_early::register_phase53_security` callers that haven't
    // migrated to the real-JDK accessors still see consistent state.
    ctx.set_field(p, 0, Value::Object(Some(n)));
    ctx.set_field(p, 1, Value::Double(version));
    ctx.set_field(p, 2, Value::Object(Some(info)));
    p
}

// ---------------------------------------------------------------------------
// Native callbacks
// ---------------------------------------------------------------------------

// WP6.5: Provider field accessors must be layout-aware. The synthetic
// Provider allocated by `make_provider` stores `name`/`version`/`info` in
// slots 0/1/2.  Real-JDK `java.security.Provider`, however, declares
// `serialVersionUID` (long, slot 0+1 — category 2), `debug` (slot 2),
// `name` (slot 3), `info` (slot 4), `version` (double, slot 5+6),
// `versionStr` (slot 7), etc.  Reading slot 0 from a real-JDK Provider
// returns the high half of `serialVersionUID`, which appears to callers
// as `null` (or whatever junk happens to be there) and made every
// `Provider.getName()` / `getVersionStr()` call lie about the receiver,
// which in turn broke `BouncyCastleProvider.setup()` (it queries its
// own name from inside `loadServiceClass` to build cache keys).
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
    Ok(Some(ctx.get_field(this, 0)))
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
    Ok(Some(ctx.get_field(this, 2)))
}

fn security_get_providers(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let chain = snapshot();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, chain.len());
    for (i, (name, ver, coverage)) in chain.iter().enumerate() {
        let p = make_provider(ctx, name, *ver, coverage);
        ctx.set_array_element(arr, i, Value::Object(Some(p)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn security_get_provider(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name_str = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    match find(&name_str) {
        Some((ver, coverage)) => {
            let p = make_provider(ctx, &name_str, ver, coverage);
            Ok(Some(Value::Object(Some(p))))
        }
        // JDK contract: return null for unknown name.
        None => Ok(Some(Value::Object(None))),
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
        "keystore.type" => "PKCS12",
        "ssl.KeyManagerFactory.algorithm" => "SunX509",
        "ssl.TrustManagerFactory.algorithm" => "PKIX",
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.create_string(val);
    Ok(Some(Value::Object(Some(s))))
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
fn aliases() -> &'static parking_lot::Mutex<FxHashMap<(String, String, String), String>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<parking_lot::Mutex<FxHashMap<(String, String, String), String>>> =
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
    // Resolve alias first.
    let canonical = aliases()
        .lock()
        .get(&(provider.to_string(), type_n.clone(), algo_n.clone()))
        .cloned()
        .unwrap_or(algo_n);
    services()
        .lock()
        .get(provider)
        .and_then(|m| m.get(&(type_n, canonical)).cloned())
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
    let entry = ServiceEntry {
        type_str: type_str.to_string(),
        algorithm: algorithm.to_string(),
        class_name: value.to_string(),
        key: format!("{type_str}.{algorithm}"),
    };
    let type_n = normalize_engine(type_str);
    let algo_n = normalize_algo(algorithm);
    let mut s = services().lock();
    s.entry(provider.to_string())
        .or_default()
        .insert((type_n, algo_n), entry);
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
    // ECDSA Signature family (DER output) + IEEE-P1363 (raw R||S) variants.
    let sigs: &[(&str, &str)] = &[
        ("NONEwithECDSA", "sun.security.ec.ECDSASignature$Raw"),
        ("SHA1withECDSA", "sun.security.ec.ECDSASignature$SHA1"),
        ("SHA224withECDSA", "sun.security.ec.ECDSASignature$SHA224"),
        ("SHA256withECDSA", "sun.security.ec.ECDSASignature$SHA256"),
        ("SHA384withECDSA", "sun.security.ec.ECDSASignature$SHA384"),
        ("SHA512withECDSA", "sun.security.ec.ECDSASignature$SHA512"),
        (
            "SHA3-224withECDSA",
            "sun.security.ec.ECDSASignature$SHA3_224",
        ),
        (
            "SHA3-256withECDSA",
            "sun.security.ec.ECDSASignature$SHA3_256",
        ),
        (
            "SHA3-384withECDSA",
            "sun.security.ec.ECDSASignature$SHA3_384",
        ),
        (
            "SHA3-512withECDSA",
            "sun.security.ec.ECDSASignature$SHA3_512",
        ),
        (
            "NONEwithECDSAinP1363Format",
            "sun.security.ec.ECDSASignature$RawinP1363Format",
        ),
        (
            "SHA1withECDSAinP1363Format",
            "sun.security.ec.ECDSASignature$SHA1inP1363Format",
        ),
        (
            "SHA224withECDSAinP1363Format",
            "sun.security.ec.ECDSASignature$SHA224inP1363Format",
        ),
        (
            "SHA256withECDSAinP1363Format",
            "sun.security.ec.ECDSASignature$SHA256inP1363Format",
        ),
        (
            "SHA384withECDSAinP1363Format",
            "sun.security.ec.ECDSASignature$SHA384inP1363Format",
        ),
        (
            "SHA512withECDSAinP1363Format",
            "sun.security.ec.ECDSASignature$SHA512inP1363Format",
        ),
    ];
    for (algo, cls) in sigs {
        put_service(P, "Signature", algo, cls);
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
    const HASHES: &[&str] = &[
        "SHA1",
        "SHA224",
        "SHA256",
        "SHA384",
        "SHA512",
        "SHA512_224",
        "SHA512_256",
    ];
    const KEYSIZES: &[&str] = &["128", "256"];
    for hash in HASHES {
        for keysize in KEYSIZES {
            let algo = format!("PBEWithHmac{hash}AndAES_{keysize}");
            // The nested class name drops the "PBEWith" prefix, e.g.
            // `PBES2Parameters$HmacSHA256AndAES_256` (verified via `javap`),
            // NOT `PBES2Parameters$PBEWithHmacSHA256AndAES_256`.
            let cls = format!("com.sun.crypto.provider.PBES2Parameters$Hmac{hash}AndAES_{keysize}");
            put_service(P, "AlgorithmParameters", &algo, &cls);
        }
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
    put_alias(J, "SSLContext", "SSL", "TLS");
    // KeyStore lives in the SUN provider (JKS/CaseExactJKS) and PKCS12 too.
    const S: &str = "SUN";
    put_service(
        S,
        "KeyStore",
        "JKS",
        "sun.security.provider.JavaKeyStore$JKS",
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
        "sun.security.pkcs12.PKCS12KeyStore",
    );
    put_alias(S, "KeyStore", "PKCS#12", "PKCS12");
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
fn put_alias(provider: &str, type_str: &str, alias: &str, canonical: &str) {
    let type_n = normalize_engine(type_str);
    let alias_n = normalize_algo(alias);
    let canon_n = normalize_algo(canonical);
    aliases()
        .lock()
        .insert((provider.to_string(), type_n, alias_n), canon_n);
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
            // Attribute on an existing service.  Ignore here — attribute
            // semantics (`SupportedModes`, `SupportedKeyClasses`, …) are
            // queried by `Service.supportsParameter` which we don't
            // intercept.  Returning true so callers can distinguish
            // "ignored shape" (false) from "recognized but no-op" (true).
            true
        }
        _ => false,
    }
}

/// Discover the provider name attached to a `Provider` receiver.  Used
/// by `Provider.put` / `parseLegacyPut` to key the global service map.
/// Falls back to `"<unknown>"` if the receiver has no name (which would
/// only happen for synthetic test fixtures).
fn provider_name_of(ctx: &dyn NativeContext, prov: ObjectRef) -> String {
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
    let val = provider_properties().lock().get(&(pname, key)).cloned();
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
    let ihash = ctx.identity_hash_code(this) as i64;
    let found = provider_instance_keys().lock().contains(&(ihash, key));
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
fn service_classname_table() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i64, String>> {
    use std::sync::OnceLock;
    static T: OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i64, String>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Allocate a `Provider$Service` synthetic populated from a stored
/// `ServiceEntry`.  Used by `provider_get_service_native` and the
/// `Cipher.getInstance(algo, providerName)` resolution path.
fn make_service(ctx: &mut dyn NativeContext, entry: &ServiceEntry, prov: ObjectRef) -> ObjectRef {
    let prov_pin = ctx.pin_native_root(prov);
    let svc0 = alloc_concurrent_synthetic(ctx, "java/security/Provider$Service", 7);
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

    // Real-JDK path — write by field name.
    ctx.set_field_by_name(svc, "provider", Value::Object(Some(prov)));
    ctx.set_field_by_name(svc, "type", Value::Object(Some(type_s)));
    ctx.set_field_by_name(svc, "algorithm", Value::Object(Some(algo_s)));
    ctx.set_field_by_name(svc, "className", Value::Object(Some(class_s)));

    // Synthetic-mode mirror — getType=0, getAlgorithm=1, getProvider=2
    // (matches the layout in `phases_early::register_phase53_security`).
    ctx.set_field(svc, 0, Value::Object(Some(type_s)));
    ctx.set_field(svc, 1, Value::Object(Some(algo_s)));
    ctx.set_field(svc, 2, Value::Object(Some(prov)));
    // Slot 3 reserved for className so the new `Service.getClassName`
    // accessor (registered below) returns the right string.
    ctx.set_field(svc, 3, Value::Object(Some(class_s)));
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
    ctx.unpin_native_roots(prov_pin);
    svc
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
            let svc = make_service(ctx, &entry, this);
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
fn provider_get_services_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let prov_name = provider_name_of(ctx, this);
    let entries: Vec<ServiceEntry> = services()
        .lock()
        .get(&prov_name)
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default();

    let this_pin = ctx.pin_native_root(this);
    let set = cratonvm_native_collections::make_hashset_with_elements(ctx, &[]);
    let set_pin = ctx.pin_native_root(set);
    for entry in entries {
        let prov = ctx.read_native_pin(this_pin, this);
        let svc = make_service(ctx, &entry, prov);
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
    ctx.unpin_native_roots(set_pin);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(set))))
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
) -> Option<ObjectRef> {
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
    let entry = entry?;
    let (ver, coverage) = find(provider).unwrap_or((25.0, USER_PROVIDER_COVERAGE));
    let prov_obj = make_provider(ctx, provider, ver, coverage);
    Some(make_service(ctx, &entry, prov_obj))
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
    match resolve_service(ctx, &provider, &type_str, &algo) {
        Some(svc) => Ok(Some(Value::Object(Some(svc)))),
        None => Err(cratonvm_types::error::RuntimeError::NotImplemented {
            feature: format!(
                "no {type_str} {algo} implementation registered for provider {provider}"
            ),
        }
        .into()),
    }
}

/// `sun.security.jca.GetInstance.getService(String type, String algorithm)` —
/// no-provider form: walk our provider chain in order and return the first
/// match (mirrors `ProviderList.getService`).
fn getinstance_get_service_search(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let type_str = read_arg_string(ctx, args, 0);
    let algo = read_arg_string(ctx, args, 1);
    for (name, _, _) in snapshot() {
        if let Some(svc) = resolve_service(ctx, &name, &type_str, &algo) {
            return Ok(Some(Value::Object(Some(svc))));
        }
    }
    Err(cratonvm_types::error::RuntimeError::NotImplemented {
        feature: format!("no {type_str} {algo} implementation registered in any provider"),
    }
    .into())
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
pub(crate) fn find_service_provider(type_str: &str, algo: &str) -> Option<String> {
    snapshot()
        .into_iter()
        .find(|(name, _, _)| get_service_entry(name, type_str, algo).is_some())
        .map(|(name, _, _)| name)
}

/// Resolve `name` to the best available `Provider` object: the REAL
/// user-registered instance if one is on file (see `real_provider_table` /
/// `remember_real_provider`), so `getInfo()`/`getName()` return exactly what
/// the caller's own `Provider` constructor set — otherwise a fresh synthetic
/// built from the seed-list entry (or a generic `USER_PROVIDER_COVERAGE`
/// synthetic if `name` isn't in the seed list at all).
pub(crate) fn resolve_or_make_provider(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    if let Some(real) = resolve_real_provider(ctx, name) {
        return real;
    }
    let (ver, coverage) = find(name).unwrap_or((1.0, USER_PROVIDER_COVERAGE));
    make_provider(ctx, name, ver, coverage)
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
) -> Option<MethodCallResult> {
    let impl_result = build_jca_impl(ctx, provider, type_str, algo)?;
    Some((|| {
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
        let prov_obj = make_provider(ctx, provider, ver, coverage);
        let impl_ref = ctx.read_native_pin(pin, impl_ref);
        // 2. Build GetInstance$Instance(provider, impl) via its real ctor.
        let inst = ctx.new_object_initialized(
            "sun/security/jca/GetInstance$Instance",
            "(Ljava/security/Provider;Ljava/lang/Object;)V",
            &[Value::Object(Some(prov_obj)), Value::Object(Some(impl_ref))],
        );
        ctx.unpin_native_roots(pin);
        inst
    })())
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
    match build_jca_instance(ctx, &provider, &type_str, &algo) {
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

fn getinstance_instance_provider_obj(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (String type, Class clazz, String algorithm, Provider provider)
    let type_str = read_arg_string(ctx, args, 0);
    let algo = read_arg_string(ctx, args, 2);
    let provider = match args.get(3) {
        Some(Value::Object(Some(p))) => read_provider_name_version(ctx, *p)
            .map(|(n, _)| n)
            .unwrap_or_default(),
        _ => String::new(),
    };
    match build_jca_instance(ctx, &provider, &type_str, &algo) {
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
        if let Some(svc) = resolve_service(ctx, &name, &type_str, &algo) {
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
        if let Some(r) = build_jca_instance(ctx, &name, &type_str, &algo) {
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
) -> Option<ObjectRef> {
    let algo = read_arg_string(ctx, args, 0);
    let provider = find_service_provider("CertificateFactory", &algo)?;
    let impl_ref = match build_jca_impl(ctx, &provider, "CertificateFactory", &algo)? {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
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
            Value::Object(Some(provider_obj)),
            Value::Object(Some(algo_str)),
        ],
    );
    ctx.unpin_native_roots(pin);
    match result {
        Ok(Some(Value::Object(Some(o)))) => Some(o),
        _ => None,
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
    if !crate::real_jca_mode() {
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
    // security decision — nothing is bypassed by answering it. CratonVM has no
    // JFR security-event subsystem, so "security logging is off" is the true
    // answer, and the real JDK returns the same `false` whenever the JFR
    // security events are not enabled.
    // jdk.internal.event.EventHelper.isLoggingSecurity() — JFR security-event
    // logging gate. Its real body dereferences the static `JUJA`
    // (`SharedSecrets.getJavaUtilJarAccess()`), which is null in our VM, so it
    // NPEs ("Cannot invoke isInitializing on null") on the
    // CertificateFactory.generateCertificate -> JCAUtil.tryCommitCertEvent
    // path. We don't emit JFR security events, so report logging-off (false).
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
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

    #[test]
    fn seed_chain_has_thirteen_jdk25_providers() {
        let chain = snapshot();
        assert_eq!(chain.len(), 13);
        let names: Vec<&str> = chain.iter().map(|(n, _, _)| n.as_str()).collect();
        // Spot-check: SUN must be first, SunPKCS11 last, SunJCE in the middle.
        assert_eq!(names[0], "SUN");
        assert_eq!(names[12], "SunPKCS11");
        assert!(names.contains(&"SunJCE"));
        assert!(names.contains(&"SunEC"));
    }

    // -----------------------------------------------------------------
    // C19 — coverage disclosure for unbacked providers.
    // -----------------------------------------------------------------

    #[test]
    fn c19_seed_chain_marks_unbacked_providers() {
        // The placeholder providers (SunPKCS11, SunMSCAPI, SunPCSC,
        // JdkLDAP, JdkSASL, SunJGSS, SunSASL, XMLDSig, SunJSSE, SunEC)
        // must all expose `COVERAGE_UNBACKED`. SUN, SunRsaSign, SunJCE
        // expose backed-coverage strings.
        for unbacked in [
            "SunPKCS11",
            "SunMSCAPI",
            "SunPCSC",
            "JdkLDAP",
            "JdkSASL",
            "SunJGSS",
            "SunSASL",
            "XMLDSig",
            "SunJSSE",
            "SunEC",
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
        for backed in ["SUN", "SunRsaSign", "SunJCE"] {
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
