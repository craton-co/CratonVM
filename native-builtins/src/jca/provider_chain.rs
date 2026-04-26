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

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::{ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, obj_arg};

// ---------------------------------------------------------------------------
// Provider chain — process-wide mutable list mirroring HotSpot's default
// JDK 25 ordering. The list is consulted by `Security.getProviders` and
// `Security.getProvider(name)`; mutations from `addProvider /
// insertProviderAt / removeProvider` persist for the lifetime of the VM.
// ---------------------------------------------------------------------------

fn provider_chain() -> &'static parking_lot::Mutex<Vec<(String, f64)>> {
    use std::sync::OnceLock;
    static CHAIN: OnceLock<parking_lot::Mutex<Vec<(String, f64)>>> = OnceLock::new();
    CHAIN.get_or_init(|| {
        // Seed list — order, names, and version match the JDK 25.0.1
        // reference HotSpot output (captured via `Security.getProviders()`
        // on a stock install). Version 25.0 — formatted as "25" by
        // `getVersionStr()` to match HotSpot's `Provider.versionStr`.
        parking_lot::Mutex::new(vec![
            ("SUN".to_string(), 25.0),
            ("SunRsaSign".to_string(), 25.0),
            ("SunEC".to_string(), 25.0),
            ("SunJSSE".to_string(), 25.0),
            ("SunJCE".to_string(), 25.0),
            ("SunJGSS".to_string(), 25.0),
            ("SunSASL".to_string(), 25.0),
            ("XMLDSig".to_string(), 25.0),
            ("SunPCSC".to_string(), 25.0),
            ("JdkLDAP".to_string(), 25.0),
            ("JdkSASL".to_string(), 25.0),
            ("SunMSCAPI".to_string(), 25.0),
            ("SunPKCS11".to_string(), 25.0),
        ])
    })
}

fn snapshot() -> Vec<(String, f64)> {
    provider_chain().lock().clone()
}

fn find(name: &str) -> Option<f64> {
    provider_chain()
        .lock()
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| *v)
}

fn add(name: String, ver: f64) -> i32 {
    let mut list = provider_chain().lock();
    if let Some(idx) = list.iter().position(|(n, _)| *n == name) {
        list[idx].1 = ver;
        return (idx + 1) as i32;
    }
    list.push((name, ver));
    list.len() as i32
}

fn insert_at(name: String, ver: f64, pos: i32) -> i32 {
    let mut list = provider_chain().lock();
    if let Some(idx) = list.iter().position(|(n, _)| *n == name) {
        return (idx + 1) as i32;
    }
    let target = if pos < 1 {
        list.len()
    } else {
        ((pos - 1) as usize).min(list.len())
    };
    list.insert(target, (name, ver));
    (target + 1) as i32
}

fn remove(name: &str) {
    let mut list = provider_chain().lock();
    if let Some(idx) = list.iter().position(|(n, _)| n == name) {
        list.remove(idx);
    }
}

// ---------------------------------------------------------------------------
// Provider synthetic — slot layout documented at module top.
// ---------------------------------------------------------------------------

/// Materialise a fresh `java.security.Provider` synthetic with name +
/// numeric version. Used by every read-side path
/// (`getProviders`, `getProvider`); we never cache `ObjectRef` values
/// across callbacks so the heap is free to GC the previous instance.
fn make_provider(ctx: &mut dyn NativeContext, name: &str, version: f64) -> ObjectRef {
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
    let p = alloc_concurrent_synthetic(ctx, "java/security/Provider", 8);
    let n = ctx.create_string(name);
    let info_str = format!("{} security provider (rust-jvm)", name);
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
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, chain.len());
    for (i, (name, ver)) in chain.iter().enumerate() {
        let p = make_provider(ctx, name, *ver);
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
        Some(ver) => {
            let p = make_provider(ctx, &name_str, ver);
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
fn read_provider_name_version(
    ctx: &dyn NativeContext,
    prov: ObjectRef,
) -> Option<(String, f64)> {
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
    Ok(Some(Value::Int(add(name, ver))))
}

fn security_insert_provider_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let prov = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let pos = args
        .get(1)
        .and_then(|v| if let Value::Int(i) = v { Some(*i) } else { None })
        .unwrap_or(1);
    let (name, ver) = match read_provider_name_version(ctx, prov) {
        Some(pair) => pair,
        None => return Ok(Some(Value::Int(-1))),
    };
    Ok(Some(Value::Int(insert_at(name, ver, pos))))
}

fn security_remove_provider(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    remove(&name);
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

fn security_set_property(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // No-op — the seeded list above is fixed; mutating it would require
    // a Properties-backed store which is out of scope for WP6.1.
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub(crate) fn register(r: &mut NativeMethodRegistry) {
    let prov = "java/security/Provider";
    r.register(prov, "getName", "()Ljava/lang/String;", provider_get_name);
    r.register(prov, "getVersion", "()D", provider_get_version);
    r.register(prov, "getVersionStr", "()Ljava/lang/String;", provider_get_version_str);
    r.register(prov, "toString", "()Ljava/lang/String;", provider_to_string);
    r.register(prov, "getInfo", "()Ljava/lang/String;", provider_get_info);

    let sec = "java/security/Security";
    r.register(sec, "getProviders", "()[Ljava/security/Provider;", security_get_providers);
    r.register(
        sec,
        "getProvider",
        "(Ljava/lang/String;)Ljava/security/Provider;",
        security_get_provider,
    );
    r.register(sec, "addProvider", "(Ljava/security/Provider;)I", security_add_provider);
    r.register(
        sec,
        "insertProviderAt",
        "(Ljava/security/Provider;I)I",
        security_insert_provider_at,
    );
    r.register(sec, "removeProvider", "(Ljava/lang/String;)V", security_remove_provider);
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
    use super::*;

    #[test]
    fn seed_chain_has_thirteen_jdk25_providers() {
        let chain = snapshot();
        assert_eq!(chain.len(), 13);
        let names: Vec<&str> = chain.iter().map(|(n, _)| n.as_str()).collect();
        // Spot-check: SUN must be first, SunPKCS11 last, SunJCE in the middle.
        assert_eq!(names[0], "SUN");
        assert_eq!(names[12], "SunPKCS11");
        assert!(names.contains(&"SunJCE"));
        assert!(names.contains(&"SunEC"));
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
}
