//! WP2.3-B — `MethodHandles.Lookup.defineClass` /
//! `defineHiddenClass` / `defineHiddenClassWithClassData`.
//!
//! These three natives are the modern (JDK 9+) entry points for runtime
//! class generation:
//!
//! * `defineClass([B)` — defines a NORMAL class (not hidden) under the
//!   lookup class's loader namespace, with the lookup class's PD
//!   inherited as the new class's CodeSource. Used by Hibernate +
//!   Weld + the JDK-internal `LambdaForm` runtime.
//!
//! * `defineHiddenClass([B, boolean, ClassOption...)` (JEP 371) — defines
//!   a HIDDEN class that is never returned by `Class.forName`. The new
//!   class can opt into being a NESTMATE of the lookup class so it has
//!   private access to the host's members. Used by ByteBuddy 1.10+,
//!   CGLIB-replacement code, and the JDK lambda runtime.
//!
//! * `defineHiddenClassWithClassData([B, Object, boolean, ClassOption...)`
//!   (JEP 371 follow-up) — same as `defineHiddenClass` but additionally
//!   stashes a Java `Object` as the new class's "class data", retrievable
//!   from inside the class via the special pseudo-constant-pool entry
//!   `MethodHandles.classData(...)`. Used by `LambdaMetafactory` to
//!   thread bootstrap arguments into a freshly-spun lambda class.
//!
//! All three:
//!   * route through `NativeContext::define_class_full` so they share
//!     the same backend as `Unsafe.defineClass` and
//!     `ClassLoader.defineClass1`;
//!   * skip bytecode verification (the JDK trusts these entry points);
//!   * inherit the lookup class's CodeSource into the new class's PD;
//!   * inherit the lookup class as the nest host for hidden classes
//!     (default, even without explicit `NESTMATE`, because JEP 371
//!     specifies that a hidden class is always a nestmate of the
//!     defining lookup class — see JLS §12.7).
//!
//! # Override semantics
//!
//! The lighter-weight implementations in `classloader.rs` (which lacks
//! the lookup-class CodeSource pull and the WithClassData variant) are
//! still registered first by `register_classloader_natives`. The
//! `register_lookup_define_class` entry point below is wired LAST, after
//! `register_classloader_natives`, so the WP2.3-B versions override the
//! older ones at the registry layer.

use std::sync::atomic::Ordering;

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{ObjectRef, Value};
use rustjvm_types::error::{MethodCallResult, RuntimeError};

use crate::obj_arg;

const LK_CLASS: &str = "java/lang/invoke/MethodHandles$Lookup";
/// Slot index of the lookup class reference within the synthetic Lookup
/// object. Must stay in sync with `classloader.rs::LK_LOOKUP_CLASS_REF`
/// (which is 0).
const LK_LOOKUP_CLASS_REF: usize = 0;

const CLASS_FILE_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];

// ---------------------------------------------------------------------------
// Common decode helpers
// ---------------------------------------------------------------------------

/// Decode a `byte[]` argument into a `Vec<u8>`. Returns
/// `IllegalArgumentException` for null / non-array values.
fn decode_byte_array(
    ctx: &mut dyn NativeContext,
    val: Option<&Value>,
    err_prefix: &str,
) -> Result<Vec<u8>, rustjvm_types::error::MethodCallFailed> {
    let arr = match val {
        Some(Value::Object(Some(a))) => *a,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("{err_prefix}: bytes must not be null"),
            }
            .into());
        }
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("{err_prefix}: missing bytes argument"),
            }
            .into());
        }
    };
    let length = ctx.array_length(arr);
    let mut out = Vec::with_capacity(length);
    for i in 0..length {
        match ctx.get_array_element(arr, i) {
            Value::Int(b) => out.push(b as u8),
            _ => out.push(0),
        }
    }
    if out.len() < 4 || out[0..4] != CLASS_FILE_MAGIC {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("{err_prefix}: not a valid class file (bad magic)"),
        }
        .into());
    }
    Ok(out)
}

/// Resolve the lookup class out of a Lookup `this`, returning its
/// internal binary name (e.g. `"java/util/HashMap"`) when known.
/// Returns `None` for the public/anonymous lookup whose lookup class is
/// unset.
fn lookup_class_name(ctx: &mut dyn NativeContext, this_lookup: ObjectRef) -> Option<String> {
    if let Value::Object(Some(mirror)) = ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF) {
        crate::lang_class::mirror_class_id(ctx, mirror)
            .and_then(|cid| ctx.class_name_of_id(cid))
    } else {
        None
    }
}

/// Pull the lookup class's `ProtectionDomain` CodeSource URL string, if
/// any. The mirror's PD lives at field-by-name `"protectionDomain"` on
/// real-JDK Class layouts; the synthetic Class layout doesn't expose a
/// PD, so this gracefully degrades to `None`.
fn lookup_class_code_source(ctx: &mut dyn NativeContext, this_lookup: ObjectRef) -> Option<String> {
    let mirror = match ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF) {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    // Real-JDK Class.protectionDomain → ProtectionDomain.codesource → CodeSource.location
    let pd = match ctx.get_field_by_name(mirror, "protectionDomain") {
        Value::Object(Some(p)) => p,
        _ => return None,
    };
    let cs = match ctx.get_field_by_name(pd, "codesource") {
        Value::Object(Some(c)) => c,
        // Synthetic-PD layout uses field index 0 directly.
        _ => match ctx.get_field(pd, 0) {
            Value::Object(Some(c)) => c,
            _ => return None,
        },
    };
    // CodeSource.location is a URL — try `getLocation` style by-name
    // first, fall back to slot 0. The URL itself stringifies via
    // `URL.toString` which for our synthetic URLs is just the stored
    // string.
    let url_obj_or_str = match ctx.get_field_by_name(cs, "location") {
        Value::Object(Some(u)) => u,
        _ => match ctx.get_field(cs, 0) {
            Value::Object(Some(u)) => u,
            _ => return None,
        },
    };
    // If it's already a String mirror, read directly. Otherwise try
    // reading slot 0 of a URL object (synthetic URL stores the string
    // form there).
    if let Some(s) = ctx.read_string(url_obj_or_str) {
        return Some(s);
    }
    if let Value::Object(Some(s)) = ctx.get_field(url_obj_or_str, 0) {
        if let Some(s) = ctx.read_string(s) {
            return Some(s);
        }
    }
    None
}

/// Allocate a fresh Lookup synthetic with full-power modes pointing at
/// the given mirror. Mirrors `classloader.rs::alloc_lookup` but uses
/// only the public `NativeContext` surface so this module stays
/// independent of `classloader.rs`.
fn alloc_lookup_for(ctx: &mut dyn NativeContext, lookup_mirror: ObjectRef) -> ObjectRef {
    // Synthetic Lookup is a 4-field allocation:
    //   slot 0: lookupClass (Class mirror)
    //   slot 1: allowedModes (int)
    //   slot 2: previousLookupClass (Class mirror | null)
    //   slot 3: lookupMode (int — duplicate, kept for layout parity)
    //
    // FULL_POWER = PUBLIC | PRIVATE | PROTECTED | PACKAGE | MODULE | ORIGINAL
    //            = 0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x40
    //            = 0x5F
    const LK_FULL_POWER: i32 = 0x5F;
    let obj = crate::alloc_concurrent_synthetic(ctx, LK_CLASS, 4);
    ctx.set_field(obj, 0, Value::Object(Some(lookup_mirror)));
    ctx.set_field(obj, 1, Value::Int(LK_FULL_POWER));
    ctx.set_field(obj, 2, Value::Object(None));
    ctx.set_field(obj, 3, Value::Int(LK_FULL_POWER));
    obj
}

// ---------------------------------------------------------------------------
// 1. Lookup.defineClass([B)Ljava/lang/Class;
// ---------------------------------------------------------------------------
//
// JLS §5.3.5 / JEP 274:
//   * The class is defined under the LOOKUP CLASS's loader.
//   * The class's name is taken from its own `this_class` constant pool
//     entry — we pass an empty string to the backend so it skips its
//     name-mismatch check (the class file's `this_class` is authoritative).
//   * The new class inherits the lookup class's PROTECTION DOMAIN.
//   * Throws IllegalArgumentException on bad magic / parse error.

fn lk_define_class_b(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this_lookup = obj_arg(args, 0)?;
    let class_bytes = decode_byte_array(ctx, args.get(1), "Lookup.defineClass")?;

    // Inherit the lookup class's PD CodeSource (URL).
    let code_source_url = lookup_class_code_source(ctx, this_lookup);

    let opts = rustjvm_native_api::DefineClassFull {
        skip_verification: true,
        code_source_url,
        ..Default::default()
    };

    match ctx.define_class_full("", &class_bytes, 0, opts) {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => Err(RuntimeError::IllegalArgumentException {
            message: format!("Lookup.defineClass: {msg}"),
        }
        .into()),
    }
}

// ---------------------------------------------------------------------------
// 2. Lookup.defineHiddenClass([B, boolean, ClassOption...) → Lookup
// ---------------------------------------------------------------------------
//
// JEP 371: the new class is HIDDEN, has the lookup class as its NEST
// HOST (always — even without the NESTMATE option, the spec says hidden
// classes are nestmates of the defining lookup class), and is named
// "<original>/0x<id>" so multiple defines from the same template get
// distinct synthetic names.

fn lk_define_hidden_class_full(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this_lookup = obj_arg(args, 0)?;
    let class_bytes = decode_byte_array(ctx, args.get(1), "defineHiddenClass")?;
    let initialize = matches!(args.get(2), Some(Value::Int(n)) if *n != 0);

    // Walk the ClassOption[] for STRONG (we don't act on it) / NESTMATE
    // (no-op since hidden classes are always nestmates of the lookup
    // class — kept here for forward-compat with future option flags).
    if let Some(Value::Object(Some(options_arr))) = args.get(3) {
        let _opt_count = ctx.array_length(*options_arr);
        // No option currently changes the WP2.3-B behaviour. Loop reserved
        // for future options (e.g. STRONG to keep the class loader pinned).
    }

    let nest_host_class_name = lookup_class_name(ctx, this_lookup);
    let code_source_url = lookup_class_code_source(ctx, this_lookup);

    // Mint a unique mangled name. The class file's own `this_class` may
    // hold a placeholder; we pass `override_name` so the backend stamps
    // the new name into the class metadata.
    let original = nest_host_class_name
        .clone()
        .unwrap_or_else(|| "HiddenClass".to_string());
    let id = crate::classloader::HIDDEN_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let hidden_name = format!("{original}/0x{id:x}");

    let opts = rustjvm_native_api::DefineClassFull {
        override_name: Some(hidden_name.clone()),
        hidden: true,
        skip_verification: true,
        code_source_url,
        nest_host_class_name,
        initialize,
        ..Default::default()
    };

    let cid = match ctx.define_class_full(&hidden_name, &class_bytes, 0, opts) {
        Ok(cid) => cid,
        Err(msg) => {
            // initialize=true failures surface as a flavour of
            // IllegalStateException so the caller observes a typed error
            // rather than a generic IAE.
            if msg.contains("initialize after define failed") {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("ExceptionInInitializerError for {hidden_name}: {msg}"),
                }
                .into());
            }
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("defineHiddenClass({hidden_name}): {msg}"),
            }
            .into());
        }
    };

    // Return a fresh Lookup whose lookup class is the new hidden class.
    let mirror = ctx.get_class_mirror(cid);
    let lookup = alloc_lookup_for(ctx, mirror);
    Ok(Some(Value::Object(Some(lookup))))
}

// ---------------------------------------------------------------------------
// 3. Lookup.defineHiddenClassWithClassData(
//        byte[] bytes,
//        Object classData,
//        boolean initialize,
//        ClassOption... options) → Lookup
// ---------------------------------------------------------------------------
//
// JEP 371 follow-up: same shape as `defineHiddenClass` but additionally
// stores `classData` for retrieval via `MethodHandles.classData(...)`
// from inside the new class. The data is held on the side and looked up
// by `ClassId`.
//
// Implementation: define the class via `define_class_full`, then attach
// the class data via `NativeContext::set_class_data` (real backend) or,
// in the mock, simply discard it — tests don't exercise classData
// retrieval.

fn lk_define_hidden_class_with_class_data(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this_lookup = obj_arg(args, 0)?;
    let class_bytes = decode_byte_array(ctx, args.get(1), "defineHiddenClassWithClassData")?;
    let class_data: Option<ObjectRef> = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let initialize = matches!(args.get(3), Some(Value::Int(n)) if *n != 0);

    // Walk ClassOption[]; same forward-compat-only loop as the plain
    // `defineHiddenClass` case above.
    if let Some(Value::Object(Some(_options_arr))) = args.get(4) {
        // No option currently changes WP2.3-B behaviour.
    }

    let nest_host_class_name = lookup_class_name(ctx, this_lookup);
    let code_source_url = lookup_class_code_source(ctx, this_lookup);

    let original = nest_host_class_name
        .clone()
        .unwrap_or_else(|| "HiddenClass".to_string());
    let id = crate::classloader::HIDDEN_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let hidden_name = format!("{original}/0x{id:x}");

    let opts = rustjvm_native_api::DefineClassFull {
        override_name: Some(hidden_name.clone()),
        hidden: true,
        skip_verification: true,
        code_source_url,
        nest_host_class_name,
        initialize,
        ..Default::default()
    };

    let cid = match ctx.define_class_full(&hidden_name, &class_bytes, 0, opts) {
        Ok(cid) => cid,
        Err(msg) => {
            if msg.contains("initialize after define failed") {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "ExceptionInInitializerError for {hidden_name}: {msg}"
                    ),
                }
                .into());
            }
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("defineHiddenClassWithClassData({hidden_name}): {msg}"),
            }
            .into());
        }
    };

    // Stash the class data. In our backend we pass it through a side
    // table keyed on ClassId. The mock NativeContext doesn't implement
    // a real `set_class_data` slot, so we use a process-wide map here
    // as a belt-and-suspenders fallback. `MethodHandles.classData(...)`
    // (the bytecode hook that retrieves it) reads from the same map.
    if let Some(data_ref) = class_data {
        store_class_data(cid, data_ref);
    }

    let mirror = ctx.get_class_mirror(cid);
    let lookup = alloc_lookup_for(ctx, mirror);
    Ok(Some(Value::Object(Some(lookup))))
}

// ---------------------------------------------------------------------------
// Class-data side store (process-wide).
// ---------------------------------------------------------------------------
//
// `defineHiddenClassWithClassData` stashes a Java object that
// `MethodHandles.classData(Lookup, String, Class)` retrieves later.
// The retrieval native (registered elsewhere) reads from this map.
//
// Concurrent access is rare (one entry per hidden class), so a Mutex-
// guarded HashMap is sufficient — no contention concerns.

use std::collections::HashMap;
use std::sync::Mutex;

fn class_data_store() -> &'static Mutex<HashMap<u32, ObjectRef>> {
    static STORE: std::sync::OnceLock<Mutex<HashMap<u32, ObjectRef>>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn store_class_data(cid: rustjvm_types::ClassId, data: ObjectRef) {
    let mut g = class_data_store()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    g.insert(cid.as_u32(), data);
}

/// Public lookup helper used by `MethodHandles.classData` retrieval
/// natives in `lang_invoke.rs` / `phases_late.rs`. Returns the stored
/// `Object` reference for the given hidden class, or `None` if no
/// `defineHiddenClassWithClassData` ever attached one.
pub fn get_class_data(cid: rustjvm_types::ClassId) -> Option<ObjectRef> {
    class_data_store()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&cid.as_u32())
        .copied()
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

/// WP2.3-B — wire the three `MethodHandles$Lookup.define*Class*` natives.
///
/// Must be called AFTER `classloader::register_classloader_natives`
/// (which installs simpler implementations for `defineClass([B)` and
/// `defineHiddenClass(...)`); the registry overwrites the older entries
/// with these WP2.3-B versions. `defineHiddenClassWithClassData` is new
/// in this module.
pub fn register_lookup_define_class(r: &mut NativeMethodRegistry) {
    let lk = LK_CLASS;

    // Lookup.defineClass([B)Ljava/lang/Class;
    r.register(lk, "defineClass", "([B)Ljava/lang/Class;", lk_define_class_b);

    // Lookup.defineHiddenClass([B,Z,[L...$ClassOption;)Lookup;
    r.register(
        lk,
        "defineHiddenClass",
        "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_define_hidden_class_full,
    );

    // Lookup.defineHiddenClassWithClassData(
    //     [B, Object, Z, [L...$ClassOption;) → Lookup
    r.register(
        lk,
        "defineHiddenClassWithClassData",
        "([BLjava/lang/Object;Z[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_define_hidden_class_with_class_data,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use rustjvm_types::ClassId;

    fn dummy_this() -> Value {
        Value::Object(None)
    }

    /// Smallest valid class file prefix: magic + minor 0 + major 65 (JDK 21+).
    /// The mock's `define_class_from_bytes` only checks the magic; downstream
    /// linking / verification is skipped because of `skip_verification`.
    fn cafebabe_minimal() -> Vec<u8> {
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE]; // magic
        b.extend_from_slice(&[0x00, 0x00]);       // minor 0
        b.extend_from_slice(&[0x00, 0x41]);       // major 65 (JDK 21)
        b.extend_from_slice(&[0x00, 0x01]);       // cp_count = 1 (no entries)
        b.extend_from_slice(&[0x00, 0x21]);       // access_flags = ACC_PUBLIC|ACC_SUPER
        b.extend_from_slice(&[0x00, 0x00]);       // this_class
        b.extend_from_slice(&[0x00, 0x00]);       // super_class
        b.extend_from_slice(&[0x00, 0x00]);       // interfaces_count
        b.extend_from_slice(&[0x00, 0x00]);       // fields_count
        b.extend_from_slice(&[0x00, 0x00]);       // methods_count
        b.extend_from_slice(&[0x00, 0x00]);       // attributes_count
        b
    }

    #[test]
    fn lookup_define_class_rejects_null_bytes() {
        let mut ctx = MockNativeContext::new();
        let lookup = ctx.alloc_object(ClassId::new(1), 4);
        let r = lk_define_class_b(
            &mut ctx,
            &[Value::Object(Some(lookup)), Value::Object(None)],
        );
        assert!(r.is_err(), "expected IAE on null bytes");
    }

    #[test]
    fn lookup_define_class_rejects_bad_magic() {
        let mut ctx = MockNativeContext::new();
        let lookup = ctx.alloc_object(ClassId::new(1), 4);
        // Allocate a byte[] with bad magic.
        let bytes = ctx.new_array(rustjvm_types::ArrayElementType::Byte, 4);
        for i in 0..4 {
            ctx.set_array_element(bytes, i, Value::Int(0));
        }
        let r = lk_define_class_b(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes)),
            ],
        );
        assert!(r.is_err(), "expected IAE on bad magic");
    }

    #[test]
    fn lookup_define_class_succeeds_on_valid_magic() {
        let mut ctx = MockNativeContext::new();
        let lookup = ctx.alloc_object(ClassId::new(1), 4);
        let class_bytes = cafebabe_minimal();
        let bytes = ctx.new_array(
            rustjvm_types::ArrayElementType::Byte,
            class_bytes.len(),
        );
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes, i, Value::Int(*b as i32));
        }
        let r = lk_define_class_b(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes)),
            ],
        )
        .unwrap();
        // Result is the new Class mirror.
        assert!(matches!(r, Some(Value::Object(Some(_)))));
    }

    #[test]
    fn define_hidden_class_returns_a_lookup() {
        let mut ctx = MockNativeContext::new();
        let lookup = ctx.alloc_object(ClassId::new(1), 4);
        let class_bytes = cafebabe_minimal();
        let bytes = ctx.new_array(
            rustjvm_types::ArrayElementType::Byte,
            class_bytes.len(),
        );
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes, i, Value::Int(*b as i32));
        }
        let r = lk_define_hidden_class_full(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes)),
                Value::Int(0),       // initialize = false
                Value::Object(None), // no options
            ],
        )
        .unwrap();
        assert!(matches!(r, Some(Value::Object(Some(_)))));
    }

    #[test]
    fn define_hidden_class_with_class_data_stashes_object() {
        let mut ctx = MockNativeContext::new();
        let lookup = ctx.alloc_object(ClassId::new(1), 4);
        let payload = ctx.alloc_object(ClassId::new(1), 1);
        let class_bytes = cafebabe_minimal();
        let bytes = ctx.new_array(
            rustjvm_types::ArrayElementType::Byte,
            class_bytes.len(),
        );
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes, i, Value::Int(*b as i32));
        }
        let _r = lk_define_hidden_class_with_class_data(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes)),
                Value::Object(Some(payload)),
                Value::Int(0),
                Value::Object(None),
            ],
        )
        .unwrap();
        // The class data store should now hold *something* — we can't
        // easily recover the exact ClassId synthesized by the mock, but
        // we can assert at least one entry exists.
        let any_entry = class_data_store()
            .lock()
            .unwrap()
            .values()
            .any(|v| *v == payload);
        assert!(any_entry, "expected payload to be stored");
    }

    #[test]
    fn registration_wires_all_three_natives() {
        let mut r = NativeMethodRegistry::new();
        register_lookup_define_class(&mut r);
        assert!(
            r.find(LK_CLASS, "defineClass", "([B)Ljava/lang/Class;")
                .is_some()
        );
        assert!(
            r.find(
                LK_CLASS,
                "defineHiddenClass",
                "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;"
            )
            .is_some()
        );
        assert!(
            r.find(
                LK_CLASS,
                "defineHiddenClassWithClassData",
                "([BLjava/lang/Object;Z[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;"
            )
            .is_some()
        );
    }
}
