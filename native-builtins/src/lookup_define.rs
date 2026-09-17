// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::obj_arg;
use cratonvm_types::error::MethodCallFailed;

const LK_CLASS: &str = "java/lang/invoke/MethodHandles$Lookup";
/// Slot index of the lookup class reference within the synthetic Lookup
/// object. Must stay in sync with `classloader.rs::LK_LOOKUP_CLASS_REF`
/// (which is 0).
const LK_LOOKUP_CLASS_REF: usize = 0;

const CLASS_FILE_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];

// ---------------------------------------------------------------------------
// Common decode helpers
// ---------------------------------------------------------------------------

/// Decode a `byte[]` argument into a `Vec<u8>`, refusing it the way the JDK
/// does.
///
/// # The refusal TYPE is the contract, not the message
///
/// `Lookup.defineClass` / `defineHiddenClass` /
/// `defineHiddenClassWithClassData` are all specified to throw
/// `NullPointerException` for a null `bytes` and `ClassFormatError` for bytes
/// that are not a ClassFile. This helper answered `IllegalArgumentException` to
/// both, at all three doors, in both modes.
///
/// That matters to exactly the code that runs through here.
/// `ClassFormatError` is an `Error`; `IllegalArgumentException` is a
/// `RuntimeException`. A bytecode generator guards its emit with
/// `catch (ClassFormatError)` — because that is what the JVM throws — so ours
/// sails past the handler and surfaces somewhere unrelated, which is the
/// three-frames-from-the-defect shape the Groovy hunt spent a day on.
///
/// The refusal a caller MUST still see as `IllegalArgumentException` — bytes
/// naming a class in another package — is not raised here; it comes back from
/// the backend, and the call sites keep their `IllegalArgumentException`
/// wrapper for it. See `lang_system::lookup_define_format_error`.
fn decode_byte_array(
    ctx: &mut dyn NativeContext,
    val: Option<&Value>,
    err_prefix: &str,
) -> Result<Vec<u8>, cratonvm_types::error::MethodCallFailed> {
    let arr = match val {
        Some(Value::Object(Some(a))) => *a,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException {
                message: Some(format!("{err_prefix}: bytes must not be null")),
            }
            .into());
        }
        _ => {
            // NOT a Java-visible case: the argument is absent from the frame,
            // which means our own dispatch handed us the wrong shape. Keep it
            // distinguishable from the null the caller really passed.
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
        return Err(cratonvm_types::error::LinkageError::ClassFormatError {
            class_name: String::new(),
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
        crate::lang_class::mirror_class_id(ctx, mirror).and_then(|cid| ctx.class_name_of_id(cid))
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
    // first, fall back to slot 0.
    let url_obj_or_str = match ctx.get_field_by_name(cs, "location") {
        Value::Object(Some(u)) => u,
        _ => match ctx.get_field(cs, 0) {
            Value::Object(Some(u)) => u,
            _ => return None,
        },
    };
    // Stringify through the shared reader. The previous code read slot 0 of
    // the location and, if that was a String, returned it — which is right
    // only for the LEGACY 6-slot synthetic URL that cached the whole spec
    // there. On a real `java.net.URL` slot 0 is `protocol`, also a String, so
    // `read_string` SUCCEEDED and a real ProtectionDomain's CodeSource
    // stringified to `"file"` or `"jar"`. Not hypothetical: this sits behind
    // a by-name `Class.protectionDomain` -> `codesource` lookup, so it fires
    // precisely when the mirror carries a REAL ProtectionDomain.
    crate::classloader::url_to_external_form(ctx, url_obj_or_str)
}

/// Resolve the loader_id to use when defining a class on behalf of the
/// given `Lookup`. CGLIB 3.4+ / Spring 5.x emit proxies via
/// `Lookup.defineClass(byte[])` / `Lookup.defineHiddenClass(...)` and
/// expect the new class to live in the SAME class-loader namespace as
/// the lookup (host) class — otherwise `Class.forName(name, false,
/// hostLoader)` (used by CGLIB's `WeakCacheKey` lookup) cannot find the
/// freshly-defined proxy, and CGLIB silently falls back to
/// `Unsafe.defineClass` against the application loader, breaking
/// `getClassLoader()` parity.
///
/// Encoding bridge:
///   * `loader_id_of_class` returns the NativeContext-side encoding
///     (0=Bootstrap, 1=Extension, 2=Application, N>=3=UserDefined(N)).
///   * `define_class_full` takes the backend encoding
///     (0=Application, N>0=UserDefined(N)).
/// Bootstrap / Extension / Application all collapse to 0 because the
/// backend `define_class_full` treats `loader_id == 0` as Application
/// (the only built-in loader namespace the backend exposes today).
fn inherit_lookup_loader(ctx: &mut dyn NativeContext, this_lookup: ObjectRef) -> u32 {
    let mirror = match ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF) {
        Value::Object(Some(m)) => m,
        _ => return 0,
    };
    let cid = match crate::lang_class::mirror_class_id(ctx, mirror) {
        Some(c) => c,
        None => return 0,
    };
    // Loader-faithful gate: a class DEFINED by a user loader records that
    // loader's object (`register_defining_loader` in `defineClass1`); inherit
    // its namespace id directly — the SAME value `defineClass1` would compute —
    // so a Lookup/hidden class defined against it lands in the same namespace.
    //
    // The legacy `loader_id_of_class` i32 path below collapses `UserDefined(n)`
    // and the built-in `Application` BOTH to 2, and the `< 3` threshold then
    // mis-routed any user loader whose namespace id is 1 or 2 (the first ids
    // `allocate_loader_id` hands out) to the Application namespace. That broke
    // Hibernate's lazy bytecode-enhancement entities: a ByteBuddy
    // ReflectionOptimizer (`<Entity>$HibernateInstantiator`) is defined via
    // `Lookup.defineClass` against the ENHANCED entity (e.g. `UserDefined(2)`),
    // but landed under Application, so its generated `new <Entity>` resolved the
    // un-enhanced global copy → SessionFactory build CCE (`<Entity>` not a
    // `PersistentAttributeInterceptable`). Gated + only fires for a user loader
    // that has actually been assigned a namespace → byte-identical gate-off.
    if crate::classloader::loader_aware_resolution() {
        if let Some(loader_obj) =
            crate::classloader::defining_loader_for(ctx.vm_identity(), cid.as_u32())
        {
            if let Some(ns) = crate::classloader::peek_loader_namespace_id(ctx, loader_obj) {
                return ns;
            }
        }
    }
    let raw = ctx.loader_id_of_class(cid);
    // Negative or 0/1/2 → Application namespace (== backend loader_id 0).
    // 3+ → UserDefined(raw) (== backend loader_id raw).
    if raw < 3 {
        0
    } else {
        raw as u32
    }
}

/// Resolve the generated class's direct supertypes through the lookup class's
/// initiating loader before definition. A lookup loader may legitimately
/// delegate a superclass to its parent, so the superclass ClassId need not be
/// defined under the generated class's own namespace. Passing the resolved
/// identities through `DefineClassFull` prevents the class manager's
/// name-only fallback from selecting an unrelated same-named copy.
/// W7-26 R1 — the two `Err(_) => return (None, None)` arms below used to catch
/// **any** failure from the lookup loader's resolution, including a real
/// pending Java throwable. `(None, None)` is the documented fall-through (the
/// class manager's name-only resolution), so a `VerifyError`, a
/// `ClassFormatError`, an `ExceptionInInitializerError` from a supertype's
/// `<clinit>`, or an `OutOfMemoryError` all read as "resolve the supertype by
/// name instead" and the generated class was defined against whatever the
/// name-only lookup found — the wrong-answer half of the species, on the path
/// every ByteBuddy/CGLIB/Hibernate proxy define takes.
///
/// `Lookup.defineClass` and `Lookup.defineHiddenClass` both declare
/// `throws LinkageError` and neither absorbs a loader's throwable, so
/// propagating is HotSpot parity. `absorb_class_absent` keeps exactly the
/// "this name is not reachable from here" half absorbed —
/// `ClassNotFoundException`, `NoClassDefFoundError`, and (see its own residual)
/// `MethodCallFailed::InternalError`, which is what the resolver returns for a
/// plain classpath miss and what an isolated loader's legitimate refusal
/// arrives as. Those are every case this helper's fallback was written for.
fn resolve_lookup_supertypes(
    ctx: &mut dyn NativeContext,
    this_lookup: ObjectRef,
    class_bytes: &[u8],
) -> Result<
    (
        Option<cratonvm_types::ClassId>,
        Option<Vec<cratonvm_types::ClassId>>,
    ),
    MethodCallFailed,
> {
    let lookup_mirror = match ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF) {
        Value::Object(Some(mirror)) => mirror,
        _ => return Ok((None, None)),
    };
    let Some(lookup_class_id) = crate::lang_class::mirror_class_id(ctx, lookup_mirror) else {
        return Ok((None, None));
    };
    let Ok(class_file) = cratonvm_reader::read_class(class_bytes) else {
        return Ok((None, None));
    };

    let superclass_id = match class_file.super_class.as_deref() {
        Some(name) => match ctx.class_id_by_name_via_referencing_class(lookup_class_id, name) {
            Ok(id) => Some(id),
            Err(failed) => {
                crate::classloader_real::absorb_class_absent(&*ctx, failed)?;
                return Ok((None, None));
            }
        },
        None => None,
    };
    let mut interface_ids = Vec::with_capacity(class_file.interfaces.len());
    for name in &class_file.interfaces {
        match ctx.class_id_by_name_via_referencing_class(lookup_class_id, name) {
            Ok(id) => interface_ids.push(id),
            Err(failed) => {
                crate::classloader_real::absorb_class_absent(&*ctx, failed)?;
                return Ok((None, None));
            }
        }
    }
    Ok((superclass_id, Some(interface_ids)))
}

/// Allocate a fresh Lookup synthetic with full-power modes pointing at
/// the given mirror. Mirrors `classloader.rs::alloc_lookup` but uses
/// only the public `NativeContext` surface so this module stays
/// independent of `classloader.rs`.
fn alloc_lookup_for(
    ctx: &mut dyn NativeContext,
    lookup_mirror: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // FULL_POWER = PUBLIC | PRIVATE | PROTECTED | PACKAGE | MODULE | ORIGINAL
    //            = 0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x40
    //            = 0x5F
    // (Measured on JDK 25: `MethodHandles.lookup().lookupModes()` == 95.)
    const LK_FULL_POWER: i32 = 0x5F;
    //
    // TWO LAYOUTS share this allocation, and they do NOT agree past slot 0:
    //
    //   synthetic `MethodHandles$Lookup` (4 fields)
    //     0 lookupClass (ref) | 1 allowedModes (int)
    //     2 previousLookupClass (ref) | 3 lookupMode (int, duplicate)
    //
    //   real JDK 25 `java.lang.invoke.MethodHandles$Lookup`
    //   (`javap -p java.lang.invoke.MethodHandles$Lookup`, instance fields in
    //    declaration order)
    //     0 lookupClass (ref) | 1 prevLookupClass (ref)
    //     2 allowedModes (int) | 3 cachedProtectionDomain (ref)
    //
    // Writing slots 1/2/3 BY INDEX therefore type-confused the real layout:
    // `Int(0x5F)` landed in the `prevLookupClass` REFERENCE slot, a null ref
    // landed in `allowedModes` (which then reads back as 0 — "no access at
    // all" in the JDK), and another `Int(0x5F)` landed in the
    // `cachedProtectionDomain` reference slot. The failure was silent: the
    // only consequence was a Lookup that reports zero modes, which every
    // access check reads as powerless.
    //
    // Write by NAME on the real layout; keep the index writes as the
    // synthetic-only fallback. The discriminator is a CLASS-side witness:
    // `resolve_field_index_by_class_id` asks the CLASS whether it declares
    // `prevLookupClass`. A fabricated stub names its fields `_f0.._fN`
    // (`ensure_synthetic_class`), so it misses there and the synthetic arm
    // runs.
    //
    // It used to be a DISJUNCTION of that witness with the older value-shape
    // test, `matches!(get_field_by_name(obj, "prevLookupClass"),
    // Value::Object(_))`, kept on the stated grounds that an absent field
    // answers `Int(0)` and so a `Value::Object` answer proved the real layout.
    // **That premise is false, and the disjunct inverted the discriminator.**
    // Production `get_field_by_name` (`vm/src/vm/vm_exec.rs`) returns
    // `Value::Object(None)` for a name it cannot resolve — the trait spells it
    // out (`native-api/src/registry.rs`: "Returns `Value::Object(None)` if the
    // field is not found"). `Int(0)` is `test_utils::MockNativeContext`'s
    // answer, which is why the unit tests below never saw this. `Object(None)`
    // matches `Value::Object(_)`, so in production the disjunct was
    // UNCONDITIONALLY TRUE and the synthetic arm was unreachable: on a
    // fabricated Lookup both `set_field_by_name` calls silently no-op, the
    // verify below then finds `allowedModes` unlanded, and the real arm's
    // `resolve_field_index_by_class_id` misses too — so the mode word was
    // written NOWHERE and `defineHiddenClass` handed back a Lookup reporting 0
    // modes. That is precisely the powerless-Lookup failure the verify step
    // was added to prevent, reintroduced through the discriminator.
    //
    // Absent and present-but-null are not merely hard to tell apart from the
    // value — they are identical. Only the class can answer.
    //
    // The by-name write is still VERIFIED below. Pinning only the positive half
    // is what made the original bug invisible: a `Lookup` whose `allowedModes`
    // never received the value reads back 0 — "no access at all" — and throws
    // nothing. If the named write did not land, fall through to the synthetic
    // indices rather than returning a powerless Lookup.
    let obj = crate::try_alloc_concurrent_synthetic(ctx, LK_CLASS, 4)?;
    // Slot 0 is `lookupClass` in BOTH layouts.
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, Value::Object(Some(lookup_mirror)));
    ctx.set_field_by_name(obj, "lookupClass", Value::Object(Some(lookup_mirror)));
    let real_layout = {
        let cid = ctx.class_id_of_object(obj);
        ctx.resolve_field_index_by_class_id(cid, "prevLookupClass")
            .is_some()
    };
    if real_layout {
        ctx.set_field_by_name(obj, "prevLookupClass", Value::Object(None));
        ctx.set_field_by_name(obj, "allowedModes", Value::Int(LK_FULL_POWER));
        // `cachedProtectionDomain` (real slot 3) is deliberately NOT written.
        // It is a lazy `volatile ProtectionDomain` cache that
        // `Lookup.lookupClassProtectionDomain()` fills on first use, so null is
        // its correct fresh value. The old index write put `Int(0x5F)` into that
        // REFERENCE slot — an integer the GC would have scanned as an oop.
    }
    // Negative half: a named write that silently did not land leaves a Lookup
    // reporting zero modes. Re-assert — but on the layout the object ACTUALLY
    // has.
    //
    // W7-7: the re-assert used to run the synthetic indices unconditionally.
    // On the real layout that is not a wrong answer, it is heap corruption:
    // slot 1 is `prevLookupClass` and slot 3 is `cachedProtectionDomain`, both
    // REFERENCES the GC scans as oops, so `Int(0x5F)` in either is a bogus
    // pointer for the collector to mark and move. It is the same defect the
    // block above was written to fix, reintroduced through the failure branch —
    // pinning only the positive half a second time. On the real layout, resolve
    // the DECLARED slot instead and write that; never a fixed index.
    let modes_landed =
        matches!(ctx.get_field_by_name(obj, "allowedModes"), Value::Int(m) if m == LK_FULL_POWER);
    if !modes_landed {
        if real_layout {
            let cid = ctx.class_id_of_object(obj);
            if let Some(slot) = ctx.resolve_field_index_by_class_id(cid, "allowedModes") {
                ctx.set_field(obj, slot, Value::Int(LK_FULL_POWER));
            }
        } else {
            ctx.set_field(obj, 1, Value::Int(LK_FULL_POWER));
            ctx.set_field(obj, 2, Value::Object(None));
            ctx.set_field(obj, 3, Value::Int(LK_FULL_POWER));
        }
    }
    Ok(obj)
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
    // CGLIB 3.4+ / Spring 5.x: inherit the lookup class's loader so the
    // generated proxy can be found by `Class.forName(name, false,
    // hostLoader)`. Without this, the proxy would always land in the
    // Application namespace and CGLIB's WeakCacheKey lookup misses.
    let loader_id = inherit_lookup_loader(ctx, this_lookup);
    let (superclass_id_override, interface_id_overrides) =
        resolve_lookup_supertypes(ctx, this_lookup, &class_bytes)?;

    let opts = cratonvm_native_api::DefineClassFull {
        skip_verification: true,
        code_source_url,
        force_loader_faithful_linking: true,
        superclass_id_override,
        interface_id_overrides,
        ..Default::default()
    };

    match ctx.define_class_full("", &class_bytes, loader_id, opts) {
        Ok(cid) => {
            // Record the exact defining ClassLoader so `Class.getClassLoader()`
            // on the freshly-defined class reports the LOOKUP class's loader
            // instead of the app-loader fallback — the same thing `defineClass1`
            // / `ClassLoader.defineClass` already do for their define paths.
            //
            // ByteBuddy's reuse-check for a generated helper is
            //   result = referenceClass.getClassLoader().loadClass(name);
            //   if (result.getClassLoader() == referenceClass.getClassLoader()) return result;
            // and CGLIB's `WeakCacheKey` compares likewise. Without a registered
            // defining loader, an already-defined Hibernate ReflectionOptimizer /
            // access-optimizer bridge reports the app loader, so ByteBuddy fails
            // the identity check and RE-defines it → duplicate-define collision
            // ("already defined by user-defined(N) loader") on the second
            // `getReflectionOptimizer` for a class sharing a mapped superclass
            // (FetchGraphTest / SpecializedEntity). Gated (loader-faithful) so
            // gate-off stays byte-identical; only a lookup class defined by a
            // user loader has a recorded defining loader to propagate.
            if crate::classloader::loader_aware_resolution() {
                if let Value::Object(Some(mirror)) = ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF)
                {
                    if let Some(lookup_cid) = crate::lang_class::mirror_class_id(ctx, mirror) {
                        if let Some(loader) = crate::classloader::defining_loader_for(
                            ctx.vm_identity(),
                            lookup_cid.as_u32(),
                        ) {
                            crate::classloader::register_defining_loader(
                                ctx.vm_identity(),
                                cid.as_u32(),
                                loader,
                            );
                        }
                    }
                }
            }
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            // The backend's failure is a `Debug` rendering of the typed error
            // it actually raised, so the FORMAT family can be recovered and
            // re-thrown with its own type (`ClassFormatError`,
            // `UnsupportedClassVersionError`, `VerifyError`). Anything else --
            // notably the "prohibited package" refusal for bytes naming a class
            // outside the lookup class's package -- keeps the
            // `IllegalArgumentException` below, which is what the JDK specifies
            // for that case.
            if let Some(e) =
                crate::lang_system::lookup_define_format_error("", "Lookup.defineClass", &msg)
            {
                return Err(e);
            }
            Err(RuntimeError::IllegalArgumentException {
                message: format!("Lookup.defineClass: {msg}"),
            }
            .into())
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Lookup.defineHiddenClass([B, boolean, ClassOption...) → Lookup
// ---------------------------------------------------------------------------
//
// JEP 371 / JLS §12.7: the new class is HIDDEN. When the caller passes
// the `NESTMATE` ClassOption, the hidden class becomes a member of the
// lookup class's NEST — i.e. its nest host equals the lookup class's
// nest host, NOT the lookup class itself when the lookup class is a
// nested class. Without NESTMATE, the hidden class is in its own nest.
//
// The hidden class is named "<original>/0x<id>" so multiple defines
// from the same template get distinct synthetic names.

/// The base name a hidden class's mangled name is built from.
///
/// HotSpot names a hidden class `<this_class>/0x<addr>`, where `this_class` is
/// the name in the SUPPLIED BYTES — not the lookup class's name. Deriving it
/// from the lookup class produced `RJdkHidden/0x1` for a `RJdkHidden$Payload`
/// class file defined through a lookup on `RJdkHidden`, failing
/// `RJdkHidden.java:97`'s `getName().startsWith("RJdkHidden$Payload/0x")`.
/// `classloader.rs:8140` already does this correctly via `extract_this_class_name`.
/// Falls back to the lookup class name, then to a constant, so bytes this
/// reader cannot parse still get a unique name from the counter suffix.
fn hidden_class_base_name(class_bytes: &[u8], lookup_name: Option<&str>) -> String {
    if let Ok(class_file) = cratonvm_reader::read_class(class_bytes) {
        let this_class = class_file.this_class.to_string();
        if !this_class.is_empty() {
            return this_class;
        }
    }
    lookup_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| "HiddenClass".to_string())
}

fn lk_define_hidden_class_full(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this_lookup = obj_arg(args, 0)?;
    let class_bytes = decode_byte_array(ctx, args.get(1), "defineHiddenClass")?;
    let initialize = matches!(args.get(2), Some(Value::Int(n)) if *n != 0);

    // WP8.11.5: Walk the ClassOption[] varargs and detect NESTMATE
    // (ordinal 0) / STRONG (ordinal 1, advisory only). Each element is
    // a synthetic ClassOption enum object whose field 0 holds its
    // ordinal — same convention as `classloader.rs::lk_define_hidden_class`.
    check_options_not_null(args.get(3), "defineHiddenClass")?;
    let nestmate = parse_nestmate_option(ctx, args.get(3));

    // WP8.11.5: When NESTMATE is set, the hidden class joins the lookup
    // class's NEST (JEP 371). The correct nest-host name is the lookup
    // class's *own* nest-host attribute when the lookup is itself a
    // nested class, OR the lookup class's name when the lookup IS its
    // own nest host.
    //
    // W3-2: without NESTMATE we leave `nest_host_class_name` unset, and
    // `None` here means SELF-NEST, not "fall back to the class file's
    // NestHost attribute". Those bytes routinely carry one — javac emits
    // `NestHost` for every nested class, and re-defining an
    // already-compiled nested class as a hidden class is the normal way to
    // reach this path — but the caller did not ask to join that nest.
    // `class_manager::hidden_class_drops_class_file_nest_host` enforces the
    // distinction for every producer of `hidden: true`, so this stays a
    // plain `None`.
    let lookup_name = lookup_class_name(ctx, this_lookup);
    let nest_host_class_name = if nestmate {
        resolve_lookup_nest_host(ctx, this_lookup, lookup_name.clone())
    } else {
        None
    };
    let code_source_url = lookup_class_code_source(ctx, this_lookup);
    // CGLIB 3.4+ / ByteBuddy / Spring 5.x: inherit the lookup class's
    // loader so private-member access checks in the new hidden class
    // resolve against the host's loader namespace. The JEP 371 spec
    // requires the hidden class to "share the run-time package" of the
    // lookup class, which it cannot if the loaders differ.
    let loader_id = inherit_lookup_loader(ctx, this_lookup);
    let (superclass_id_override, interface_id_overrides) =
        resolve_lookup_supertypes(ctx, this_lookup, &class_bytes)?;
    // Keep the lookup class name only as the FALLBACK label.
    let nest_host_class_name_for_label = lookup_name;

    // Mint a unique mangled name from the class file's own `this_class` (the
    // name HotSpot uses); we pass `override_name` so the backend stamps the
    // mangled name into the class metadata.
    let original = hidden_class_base_name(&class_bytes, nest_host_class_name_for_label.as_deref());
    let id = crate::classloader::HIDDEN_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let hidden_name = format!("{original}/0x{id:x}");

    let opts = cratonvm_native_api::DefineClassFull {
        override_name: Some(hidden_name.clone()),
        hidden: true,
        skip_verification: true,
        code_source_url,
        nest_host_class_name,
        initialize,
        force_loader_faithful_linking: true,
        superclass_id_override,
        interface_id_overrides,
        ..Default::default()
    };

    let cid = match ctx.define_class_full(&hidden_name, &class_bytes, loader_id, opts) {
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
            // The backend's failure is a `Debug` rendering of the typed error
            // it actually raised, so the FORMAT family can be recovered and
            // re-thrown with its own type (`ClassFormatError`,
            // `UnsupportedClassVersionError`, `VerifyError`). Anything else --
            // notably the "prohibited package" refusal for bytes naming a class
            // outside the lookup class's package -- keeps the
            // `IllegalArgumentException` below, which is what the JDK specifies
            // for that case.
            if let Some(e) = crate::lang_system::lookup_define_format_error(
                &hidden_name,
                "defineHiddenClass",
                &msg,
            ) {
                return Err(e);
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
    Ok(Some(Value::Object(Some(lookup?))))
}

// ---------------------------------------------------------------------------
// WP8.11.5 helpers — ClassOption[] parsing and nest-host resolution.
// ---------------------------------------------------------------------------

/// `MethodHandles.Lookup.ClassOption.NESTMATE.flag` — JEP 371's
/// `NESTMATE_CLASS`. Kept in sync with `classloader.rs`'s
/// `DEFINE_CLASS0_FLAG_NESTMATE` (that one is private to its module).
const CLASS_OPTION_FLAG_NESTMATE: i32 = 0x01;

/// Is this `MethodHandles$Lookup$ClassOption` the `NESTMATE` constant?
///
/// # Two layouts, and why reading slot 0 is not enough
///
/// Under `synthetic-jdk` a `ClassOption` is a bare synthetic mirror whose
/// field 0 holds the enum ORDINAL directly (`NESTMATE = 0`, `STRONG = 1`) —
/// the convention `classloader.rs::lk_define_hidden_class` documents.
///
/// Under `--real-jdk` the array elements are REAL JDK enum constants, and
/// this native is deliberately registered for that mode too
/// (`reflect_annotations.rs`, so the real `Lookup.defineClass` bytecode does
/// not descend into `defineClass1` with a 0-length byte view). A real
/// constant is a `java.lang.Enum` subclass instance, so its slot 0 is
/// `Enum.name` — a `String` reference — with `ordinal` at 1, `hash` at 2 and
/// `ClassOption.flag` only at 3 (`javap -p java.lang.Enum` on JDK 25).
/// Reading slot 0 as an `i32` therefore never matched, `nestmate` was always
/// `false`, and `nest_host_class_name` was left `None` for BOTH arms.
///
/// That silence was invisible because it cancelled out: with no explicit nest
/// host, `class_manager` fell back to the class file's own `NestHost`
/// attribute, which for the `RJdkHidden$Payload` bytes happens to be exactly
/// the answer the NESTMATE arm wanted. `RJdkHidden.java:101` passed for the
/// wrong reason and `:151` failed for the right one — the two are one bug,
/// and fixing only the `class_manager` half would have moved the failure
/// backwards onto `:101`.
///
/// Each step below decides only on POSITIVE evidence and otherwise falls
/// through, so an unrecognised shape degrades to the historical behaviour
/// rather than guessing. The flag step requiring a NON-ZERO value is part of
/// that, though not for the reason this note used to give ("a by-name read of
/// an ABSENT field answers `Int(0)`" — that is `MockNativeContext`; production
/// answers `Value::Object(None)`, which the `Value::Int` pattern rejects
/// outright). The reason that survives both contexts is that a PRESENT but
/// never-written `int` slot decodes as `Int(0)`, and so does a fabricated
/// stub's slot under the mock — while both real constants carry a non-zero
/// flag (`NESTMATE = 0x1`, `STRONG = 0x4`). So `Int(0)` is never positive
/// evidence, and step 3 below is the one that reads a zero, deliberately, as
/// the synthetic ORDINAL rather than as a flag.
fn class_option_is_nestmate(ctx: &mut dyn NativeContext, opt: ObjectRef) -> bool {
    // 1. Real-JDK layout, primary witness: the enum constant's own name.
    if let Value::Object(Some(name_ref)) = ctx.get_field_by_name(opt, "name") {
        if let Some(name) = ctx.read_string(name_ref) {
            if name == "NESTMATE" {
                return true;
            }
            if name == "STRONG" {
                return false;
            }
        }
    }
    // 2. Real-JDK layout, second witness: `ClassOption.flag` is the JEP 371
    //    bit itself, so this also survives a rename of the constant.
    if let Value::Int(flag) = ctx.get_field_by_name(opt, "flag") {
        if flag != 0 {
            return (flag & CLASS_OPTION_FLAG_NESTMATE) != 0;
        }
    }
    // 3. Synthetic layout: field 0 IS the ordinal, and NESTMATE is 0.
    matches!(ctx.get_field(opt, 0), Value::Int(0))
}

/// Walk a `MethodHandles$Lookup$ClassOption[]` array and return `true`
/// if any element is the `NESTMATE` constant. See
/// [`class_option_is_nestmate`] for the two element layouts this must cope
/// with.
///
/// Returns `false` when the argument is null, missing, or not an array,
/// matching the JDK's behaviour for an empty `ClassOption...` varargs.
/// Refuse a null `ClassOption[]` the way the JDK does.
///
/// `defineHiddenClass(bytes, initialize, options)` throws
/// `NullPointerException` when `options` is null -- a DIFFERENT argument from
/// the bytes, and a different failure from a null element inside a present
/// array. [`parse_nestmate_option`] treats null and empty alike and answers
/// `false`, so a caller that passed `(ClassOption[]) null` while meaning
/// `NESTMATE` got a hidden class in its OWN nest and no error at all: the
/// symptom is a private-access `IllegalAccessError` from the generated class,
/// nowhere near the call that dropped the option.
fn check_options_not_null(
    opts_arg: Option<&Value>,
    method: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if matches!(opts_arg, Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(format!("{method}: options must not be null")),
        }
        .into());
    }
    Ok(())
}

fn parse_nestmate_option(ctx: &mut dyn NativeContext, opts_arg: Option<&Value>) -> bool {
    let options_arr = match opts_arg {
        Some(Value::Object(Some(arr))) => *arr,
        _ => return false,
    };
    let opt_count = ctx.array_length(options_arr);
    for i in 0..opt_count {
        if let Value::Object(Some(opt)) = ctx.get_array_element(options_arr, i) {
            if class_option_is_nestmate(ctx, opt) {
                return true;
            }
        }
    }
    false
}

/// Resolve the nest host that a NESTMATE hidden class should join.
///
/// Per JEP 371 / JLS §12.7, the new class is added to the *nest of the
/// lookup class*. When the lookup class is itself a nested class
/// (i.e. has a `NestHost` attribute pointing at an outer class), the
/// hidden class's nest host must be that outer class — otherwise
/// `Class.getNestHost()` of the hidden class would diverge from
/// `Class.getNestHost()` of the lookup class, breaking
/// `MethodHandles.privateLookupIn` and every `@Inject` injection point
/// in Weld CDI's `Bean<?>` proxies.
///
/// Resolution order:
///   1. `lookup_class.nest_host` (its own NestHost attribute) when set;
///   2. otherwise, the lookup class's own name (it IS its own nest host).
///
/// Returns `None` only when the lookup class can't be identified
/// (anonymous / public Lookup), in which case the backend falls back
/// to the class file's NestHost attribute or self-nest.
fn resolve_lookup_nest_host(
    ctx: &mut dyn NativeContext,
    this_lookup: ObjectRef,
    lookup_class_name_cached: Option<String>,
) -> Option<String> {
    let mirror = match ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF) {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let cid = crate::lang_class::mirror_class_id(ctx, mirror)?;
    // Prefer the lookup class's own NestHost attribute — this is the
    // "lookup class is itself a nestmate" case (e.g. inner-class
    // lookup minted via `MethodHandles.privateLookupIn(Outer$Inner.class, ...)`).
    if let Some(host) = ctx.nest_host_name(cid) {
        return Some(host);
    }
    // Lookup class is its own nest host.
    lookup_class_name_cached.or_else(|| ctx.class_name_of_id(cid))
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

    // WP8.11.5: NESTMATE option propagation, mirror of
    // `lk_define_hidden_class_full`. ClassOption[] is arg 4 here
    // (after [B, Object, Z); arg 3 in the plain variant).
    check_options_not_null(args.get(4), "defineHiddenClassWithClassData")?;
    let nestmate = parse_nestmate_option(ctx, args.get(4));
    let lookup_name = lookup_class_name(ctx, this_lookup);
    let nest_host_class_name = if nestmate {
        resolve_lookup_nest_host(ctx, this_lookup, lookup_name.clone())
    } else {
        None
    };
    let code_source_url = lookup_class_code_source(ctx, this_lookup);
    // Same loader-inheritance fix as the plain variant — LambdaMetafactory
    // and CGLIB classData-bound proxies both rely on it.
    let loader_id = inherit_lookup_loader(ctx, this_lookup);
    let (superclass_id_override, interface_id_overrides) =
        resolve_lookup_supertypes(ctx, this_lookup, &class_bytes)?;
    let nest_host_class_name_for_label = lookup_name;

    // Same rule as the plain variant: the label comes from the class file's
    // own `this_class`, with the lookup class name as the fallback.
    let original = hidden_class_base_name(&class_bytes, nest_host_class_name_for_label.as_deref());
    let id = crate::classloader::HIDDEN_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let hidden_name = format!("{original}/0x{id:x}");

    let opts = cratonvm_native_api::DefineClassFull {
        override_name: Some(hidden_name.clone()),
        hidden: true,
        skip_verification: true,
        code_source_url,
        nest_host_class_name,
        initialize,
        force_loader_faithful_linking: true,
        superclass_id_override,
        interface_id_overrides,
        ..Default::default()
    };

    let cid = match ctx.define_class_full(&hidden_name, &class_bytes, loader_id, opts) {
        Ok(cid) => cid,
        Err(msg) => {
            if msg.contains("initialize after define failed") {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("ExceptionInInitializerError for {hidden_name}: {msg}"),
                }
                .into());
            }
            // The backend's failure is a `Debug` rendering of the typed error
            // it actually raised, so the FORMAT family can be recovered and
            // re-thrown with its own type (`ClassFormatError`,
            // `UnsupportedClassVersionError`, `VerifyError`). Anything else --
            // notably the "prohibited package" refusal for bytes naming a class
            // outside the lookup class's package -- keeps the
            // `IllegalArgumentException` below, which is what the JDK specifies
            // for that case.
            if let Some(e) = crate::lang_system::lookup_define_format_error(
                &hidden_name,
                "defineHiddenClassWithClassData",
                &msg,
            ) {
                return Err(e);
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
    Ok(Some(Value::Object(Some(lookup?))))
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

// Round-9 MED-2: migrated `class_data_store` from `std::sync::Mutex` to
// `parking_lot::Mutex` — removes poison handling and yields a smaller, faster
// lock. The map is keyed by `ClassId` (a `u32`), which is GC-stable, so no
// further key change is required.
use parking_lot::Mutex;
use std::collections::HashMap;

// GC note (gc-followups-20260706): the stored `ObjectRef`s are neither GC
// roots nor remapped after a move, so any read after a moving GC would be
// unsound (stale address) and the object is collectable. This is currently
// tolerated ONLY because the read helper below (`get_class_data`) has no
// callers anywhere in the workspace — the table is effectively write-only.
// Before wiring a real `MethodHandles.classData` retrieval native to it,
// convert entries to the `(identity_key, ObjectRef)` var-handle-root pattern
// (see `ASYNC_POOL` in lib.rs): `register_var_handle_root` at store,
// `read_var_handle_root` at every read.
fn class_data_store() -> &'static Mutex<HashMap<u32, ObjectRef>> {
    static STORE: std::sync::OnceLock<Mutex<HashMap<u32, ObjectRef>>> = std::sync::OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn store_class_data(cid: cratonvm_types::ClassId, data: ObjectRef) {
    let mut g = class_data_store().lock();
    g.insert(cid.as_u32(), data);
}

/// Public lookup helper used by `MethodHandles.classData` retrieval
/// natives in `lang_invoke.rs` / `phases_late.rs`. Returns the stored
/// `Object` reference for the given hidden class, or `None` if no
/// `defineHiddenClassWithClassData` ever attached one.
pub fn get_class_data(cid: cratonvm_types::ClassId) -> Option<ObjectRef> {
    class_data_store().lock().get(&cid.as_u32()).copied()
}

// ---------------------------------------------------------------------------
// 4. Lookup.findClass(String) → Class<?>
// ---------------------------------------------------------------------------
//
// JDK 25 `java.lang.invoke.MethodHandles$Lookup.findClass` is pure Java:
//
//     Class<?> targetClass = Class.forName(targetName, false, lookupClass().getClassLoader());
//     return accessClass(targetClass);
//
// but `MethodHandles$Lookup` is a VM-fabricated synthetic object here (see
// the module doc above), not the real JDK class file, so there is no
// bytecode body for the interpreter to run — `findClass` was never wired to
// anything and dispatch found no method at all. See
// `docs/known-issues/quarkus/method-handles-lookup-find-class.md`
// (`ClassLoadingChainAnalyzerTest.analyzeFindsClassesLoadedViaMethodHandlesFindClass`).
//
// Reimplemented natively by delegating to the SAME `loader.loadClass(name)`
// routing `Class.forName(name, boolean, ClassLoader)` already uses
// (`native_class_for_name`) — this is what makes the call observable to a
// caller's own `ClassLoader.loadClass` override (e.g. a recording
// classloader), exactly as real bytecode calling through `Class.forName`
// would be.
fn lk_find_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this_lookup = obj_arg(args, 0)?;
    let name_arg = args.get(1).copied().unwrap_or(Value::Object(None));

    // `lookupClass().getClassLoader()` — resolve the actual ClassLoader
    // object (or null == bootstrap), exactly as `Class.getClassLoader()`
    // would, so routing below observes the same loader identity/override
    // bytecode a real `Class.forName(name, false, loader)` would.
    let loader_value = match ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF) {
        Value::Object(Some(mirror)) => {
            match crate::lang_class::native_class_get_class_loader(
                ctx,
                &[Value::Object(Some(mirror))],
            )? {
                Some(v) => v,
                None => Value::Object(None),
            }
        }
        // Public/anonymous lookup with no lookup class set — treat as the
        // bootstrap loader, matching `Object.class.getClassLoader()` (the
        // JDK's `publicLookup()` lookup class).
        _ => Value::Object(None),
    };
    if std::env::var("CRATONVM_DBG_FINDCLASS").is_ok() {
        let lookup_name = lookup_class_name(ctx, this_lookup);
        let loader_desc = match loader_value {
            Value::Object(Some(l)) => {
                let lc = ctx.class_id_of_object(l);
                format!("{}@{l:?}", ctx.class_name_of_id(lc).unwrap_or_default())
            }
            _ => "null".to_string(),
        };
        let recorded = match ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF) {
            Value::Object(Some(mirror)) => {
                crate::lang_class::mirror_class_id(ctx, mirror).map(|cid| {
                    (
                        cid.as_u32(),
                        crate::classloader::defining_loader_for(ctx.vm_identity(), cid.as_u32()),
                    )
                })
            }
            _ => None,
        };
        eprintln!(
            "[FINDCLASS-DBG] lookup_class={lookup_name:?} loader={loader_desc} recorded_defining_loader={recorded:?}"
        );
    }

    // Delegate to the same machinery backing `Class.forName(name, boolean,
    // ClassLoader)`. Argument layout per JDK 25
    // `Class.forName0(name, initialize, loader, caller)`; `initialize=false`
    // matches `findClass`'s contract (resolve/link only, never run
    // `<clinit>`) and an explicit loader value (possibly `Object(None)` for
    // the bootstrap loader) at index 2 routes through the loader-aware path
    // rather than the caller-sensitive one-arg fallback.
    let forname_args = [name_arg, Value::Int(0), loader_value];
    let target = crate::lang_class::native_class_for_name(ctx, &forname_args)?;
    let target_mirror = match target {
        Some(Value::Object(Some(m))) => m,
        _ => return Ok(target),
    };

    // `accessClass` — a public target is always reachable; a non-public
    // target additionally requires the lookup class and target to share a
    // runtime package, approximating the PACKAGE lookup mode every
    // full-power `MethodHandles.lookup()` caller carries.
    if !crate::lang_class::mirror_is_public(ctx, target_mirror) {
        let lookup_name = lookup_class_name(ctx, this_lookup);
        let target_name = crate::lang_class::mirror_class_id(ctx, target_mirror)
            .and_then(|cid| ctx.class_name_of_id(cid));
        let same_package = match (&lookup_name, &target_name) {
            (Some(l), Some(t)) => l.rfind('/').map(|i| &l[..i]) == t.rfind('/').map(|i| &t[..i]),
            _ => false,
        };
        if !same_package {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalAccessException {
                    message: format!(
                        "class {} cannot access class {}",
                        lookup_name.unwrap_or_default().replace('/', "."),
                        target_name.unwrap_or_default().replace('/', ".")
                    ),
                }
                .into(),
            );
        }
    }

    Ok(Some(Value::Object(Some(target_mirror))))
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
    r.register(
        lk,
        "defineClass",
        "([B)Ljava/lang/Class;",
        lk_define_class_b,
    );

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

    // Lookup.findClass(String)Ljava/lang/Class;
    r.register(
        lk,
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        lk_find_class,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ClassId;

    fn dummy_this() -> Value {
        Value::Object(None)
    }

    /// The EMPTY `ClassOption[]` a varargs call site actually passes. javac
    /// emits `new ClassOption[0]` for `defineHiddenClass(bytes, initialize)`;
    /// a null in that slot means the caller wrote `(ClassOption[]) null`, which
    /// the JDK answers with NullPointerException. These tests used to pass a
    /// null and so were exercising a frame shape no Java call site produces.
    fn no_options(ctx: &mut MockNativeContext) -> cratonvm_types::ObjectRef {
        ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0)
    }

    /// Smallest valid class file prefix: magic + minor 0 + major 65 (JDK 21+).
    /// The mock's `define_class_from_bytes` only checks the magic; downstream
    /// linking / verification is skipped because of `skip_verification`.
    fn cafebabe_minimal() -> Vec<u8> {
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE]; // magic
        b.extend_from_slice(&[0x00, 0x00]); // minor 0
        b.extend_from_slice(&[0x00, 0x41]); // major 65 (JDK 21)
        b.extend_from_slice(&[0x00, 0x05]); // cp_count = 5
        b.push(1); // #1 Utf8 Generated
        b.extend_from_slice(&[0x00, 0x09]);
        b.extend_from_slice(b"Generated");
        b.push(7); // #2 Class #1
        b.extend_from_slice(&[0x00, 0x01]);
        b.push(1); // #3 Utf8 java/lang/Object
        b.extend_from_slice(&[0x00, 0x10]);
        b.extend_from_slice(b"java/lang/Object");
        b.push(7); // #4 Class #3
        b.extend_from_slice(&[0x00, 0x03]);
        b.extend_from_slice(&[0x00, 0x21]); // access_flags = ACC_PUBLIC|ACC_SUPER
        b.extend_from_slice(&[0x00, 0x02]); // this_class
        b.extend_from_slice(&[0x00, 0x04]); // super_class
        b.extend_from_slice(&[0x00, 0x00]); // interfaces_count
        b.extend_from_slice(&[0x00, 0x00]); // fields_count
        b.extend_from_slice(&[0x00, 0x00]); // methods_count
        b.extend_from_slice(&[0x00, 0x00]); // attributes_count
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
        // The TYPE is the contract here, and the previous assertion --
        // `is_err()` with a message naming an exception it never checked --
        // would have stayed green through the whole defect. `Lookup.defineClass`
        // is specified to throw NullPointerException for null bytes.
        assert!(
            matches!(
                r,
                Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        RuntimeError::NullPointerException { .. }
                    )
                ))
            ),
            "null bytes must be NullPointerException, got {r:?}"
        );
    }

    #[test]
    fn lookup_define_class_rejects_bad_magic() {
        let mut ctx = MockNativeContext::new();
        let lookup = ctx.alloc_object(ClassId::new(1), 4);
        // Allocate a byte[] with bad magic.
        let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        for i in 0..4 {
            ctx.set_array_element(bytes, i, Value::Int(0));
        }
        let r = lk_define_class_b(
            &mut ctx,
            &[Value::Object(Some(lookup)), Value::Object(Some(bytes))],
        );
        // Bad magic is a CLASS FORMAT problem, and `ClassFormatError` is an
        // `Error`: a generator guarding its emit with `catch (ClassFormatError)`
        // never sees an `IllegalArgumentException`, so the malformed class
        // escapes and fails somewhere unrelated.
        assert!(
            matches!(
                r,
                Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Linkage(
                        cratonvm_types::error::LinkageError::ClassFormatError { .. }
                    )
                ))
            ),
            "bad magic must be ClassFormatError, got {r:?}"
        );
    }

    /// A null `ClassOption[]` is a different argument from null bytes, and
    /// `parse_nestmate_option` used to answer `false` for it -- silently
    /// dropping a NESTMATE the caller asked for.
    #[test]
    fn define_hidden_class_rejects_null_options() {
        let mut ctx = MockNativeContext::new();
        let lookup = ctx.alloc_object(ClassId::new(1), 4);
        let class_bytes = cafebabe_minimal();
        let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes, i, Value::Int(*b as i32));
        }
        let opts = no_options(&mut ctx);
        let r = lk_define_hidden_class_full(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes)),
                Value::Int(0),
                Value::Object(None),
            ],
        );
        assert!(
            matches!(
                r,
                Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        RuntimeError::NullPointerException { .. }
                    )
                ))
            ),
            "null options must be NullPointerException, got {r:?}"
        );
    }

    #[test]
    fn lookup_define_class_succeeds_on_valid_magic() {
        let mut ctx = MockNativeContext::new();
        let lookup = ctx.alloc_object(ClassId::new(1), 4);
        let class_bytes = cafebabe_minimal();
        let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes, i, Value::Int(*b as i32));
        }
        let r = lk_define_class_b(
            &mut ctx,
            &[Value::Object(Some(lookup)), Value::Object(Some(bytes))],
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
        let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes, i, Value::Int(*b as i32));
        }
        let opts = no_options(&mut ctx);
        let r = lk_define_hidden_class_full(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes)),
                Value::Int(0), // initialize = false
                Value::Object(Some(opts)),
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
        let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes, i, Value::Int(*b as i32));
        }
        let opts = no_options(&mut ctx);
        let _r = lk_define_hidden_class_with_class_data(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes)),
                Value::Object(Some(payload)),
                Value::Int(0),
                Value::Object(Some(opts)),
            ],
        )
        .unwrap();
        // The class data store should now hold *something* — we can't
        // easily recover the exact ClassId synthesized by the mock, but
        // we can assert at least one entry exists.
        let any_entry = class_data_store().lock().values().any(|v| *v == payload);
        assert!(any_entry, "expected payload to be stored");
    }

    #[test]
    fn registration_wires_all_three_natives() {
        let mut r = NativeMethodRegistry::new();
        register_lookup_define_class(&mut r);
        assert!(r
            .find(LK_CLASS, "defineClass", "([B)Ljava/lang/Class;")
            .is_some());
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

    // -----------------------------------------------------------------
    // WP8.11.5 — NESTMATE flag propagation tests
    // -----------------------------------------------------------------
    //
    // These tests pin the JEP 371 / JLS §12.7 contract for hidden-class
    // nest-host inheritance. The WP8.11 diagnostic agent flagged that
    // EJBCA's Weld CDI `Bean<?>` proxy generation was failing every
    // `@Inject` injection point with `IllegalAccessError` because the
    // NESTMATE option from `defineHiddenClass(bytes, true, NESTMATE,
    // STRONG)` was being collapsed to a no-op rather than propagated to
    // `define_class_full`'s `nest_host_class_name` field.
    //
    // The fix in this file walks the ClassOption[] varargs and only
    // sets `nest_host_class_name` when NESTMATE is requested; the
    // resolved name is the *lookup class's nest host* (not the lookup
    // class itself) so a hidden class defined from inside an
    // `Outer$Inner` lookup correctly joins `Outer`'s nest.

    /// Build the smallest valid `MethodHandles$Lookup$ClassOption` enum
    /// mirror with the requested ordinal at field 0 (NESTMATE = 0,
    /// STRONG = 1). Mirrors the convention used by
    /// `classloader.rs::lk_define_hidden_class`.
    fn make_class_option(ctx: &mut MockNativeContext, ordinal: i32) -> ObjectRef {
        let opt_cid = ctx
            .ensure_class_initialized("java/lang/invoke/MethodHandles$Lookup$ClassOption")
            .expect("alloc class option cid");
        let opt = ctx.alloc_object(opt_cid, 4);
        ctx.set_field(opt, 0, Value::Int(ordinal));
        opt
    }

    /// Build a `ClassOption[]` array containing the given ordinals.
    fn make_options_array(ctx: &mut MockNativeContext, ordinals: &[i32]) -> ObjectRef {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, ordinals.len());
        for (i, ord) in ordinals.iter().enumerate() {
            let opt = make_class_option(ctx, *ord);
            ctx.set_array_element(arr, i, Value::Object(Some(opt)));
        }
        arr
    }

    /// Allocate a Lookup whose lookup-class field points at a mirror of
    /// `class_name`, registering the name under a fresh ClassId.
    /// Returns `(lookup_object, lookup_class_id)`.
    fn make_lookup_for(ctx: &mut MockNativeContext, class_name: &str) -> (ObjectRef, ClassId) {
        let cid = ctx.ensure_class_initialized(class_name).expect("alloc cid");
        let mirror = ctx.get_class_mirror(cid);
        let lookup = ctx.alloc_object(ClassId::new(1), 4);
        ctx.set_field(lookup, LK_LOOKUP_CLASS_REF, Value::Object(Some(mirror)));
        (lookup, cid)
    }

    /// Drive `defineHiddenClass([B, true, options...)` with the given
    /// lookup and ClassOption ordinals, returning the captured
    /// `DefineClassFull.nest_host_class_name` actually passed to the
    /// backend. Asserts the call itself succeeded.
    fn drive_hidden_define(
        lookup_class_name: &str,
        lookup_nest_host: Option<&str>,
        option_ordinals: &[i32],
    ) -> Option<String> {
        let mut ctx = MockNativeContext::new();
        let (lookup, lookup_cid) = make_lookup_for(&mut ctx, lookup_class_name);
        if let Some(host) = lookup_nest_host {
            ctx.set_nest_host_override(lookup_cid, host);
        }

        let class_bytes = cafebabe_minimal();
        let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
        }

        let opts_arr = make_options_array(&mut ctx, option_ordinals);

        let opts = no_options(&mut ctx);
        let r = lk_define_hidden_class_full(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes_arr)),
                Value::Int(0), // initialize = false
                Value::Object(Some(opts_arr)),
            ],
        );
        assert!(r.is_ok(), "defineHiddenClass must succeed: {:?}", r.err());

        ctx.last_define_full_opts()
            .expect("define_class_full was not invoked")
            .nest_host_class_name
    }

    /// Acceptance #1: when the lookup class IS its own nest host
    /// (no `NestHost` attribute) and NESTMATE is requested, the hidden
    /// class's nest host is the lookup class itself.
    #[test]
    fn nestmate_propagates_when_lookup_is_own_nest_host() {
        let nh = drive_hidden_define(
            "weld/cdi/BeanFactory",
            None, // lookup IS its own nest host
            &[0], // NESTMATE
        );
        assert_eq!(
            nh.as_deref(),
            Some("weld/cdi/BeanFactory"),
            "NESTMATE-only hidden class must inherit the lookup class as nest host"
        );
    }

    /// Acceptance #2: when the lookup class is itself a nestmate of an
    /// outer class (i.e. its own `NestHost` attribute names the outer),
    /// the hidden class's nest host is the OUTER class — not the
    /// (already-nested) lookup class. This is the JEP 371 / JLS §12.7
    /// transitive case that broke EJBCA's `Bean<?>` proxies: Weld's
    /// `BeanFactory` is a static inner of `BeanManagerImpl`, so the
    /// proxy must end up in `BeanManagerImpl`'s nest, not in
    /// `BeanFactory`'s (which would not even be a valid nest).
    #[test]
    fn nestmate_inherits_outer_when_lookup_is_a_nestmate() {
        let nh = drive_hidden_define(
            "weld/cdi/BeanManagerImpl$BeanFactory",
            Some("weld/cdi/BeanManagerImpl"), // lookup is itself a nestmate
            &[0],                             // NESTMATE
        );
        assert_eq!(
            nh.as_deref(),
            Some("weld/cdi/BeanManagerImpl"),
            "NESTMATE on a nestmate-lookup must resolve to the OUTER nest host \
             (JEP 371: hidden class joins lookup class's nest, not the lookup itself)"
        );
    }

    /// Acceptance #3: when NESTMATE is absent from the ClassOption[]
    /// array (e.g. only STRONG passed, or empty options), the backend
    /// receives `nest_host_class_name = None` so the class file's own
    /// `NestHost` attribute (or self-nest) determines the nest host.
    /// This guarantees we don't regress the default case where a
    /// hidden class should remain in its own nest.
    #[test]
    fn no_nestmate_means_no_nest_host_inheritance() {
        // Empty options.
        let nh_empty = drive_hidden_define("some/Caller", None, &[]);
        assert_eq!(
            nh_empty, None,
            "empty ClassOption[] must NOT inherit nest host"
        );

        // STRONG-only (ordinal 1) — also must not trigger inheritance.
        let nh_strong = drive_hidden_define("some/Caller", None, &[1]);
        assert_eq!(
            nh_strong, None,
            "STRONG-only ClassOption[] must NOT inherit nest host"
        );
    }

    /// Defence-in-depth: NESTMATE + STRONG together (the exact pattern
    /// emitted by Weld CDI's proxy generator) propagates correctly.
    /// This pins the original Weld bytecode pattern from the WP8.11
    /// diagnostic.
    #[test]
    fn nestmate_plus_strong_still_propagates() {
        let nh = drive_hidden_define(
            "weld/cdi/BeanManagerImpl$BeanFactory",
            Some("weld/cdi/BeanManagerImpl"),
            &[0, 1], // NESTMATE, STRONG — exact Weld pattern
        );
        assert_eq!(
            nh.as_deref(),
            Some("weld/cdi/BeanManagerImpl"),
            "NESTMATE + STRONG (Weld's exact ClassOption[] pattern) must \
             propagate the outer nest host"
        );
    }

    // -----------------------------------------------------------------
    // W3-2 — the REAL-JDK ClassOption layout
    // -----------------------------------------------------------------
    //
    // Every test above builds a SYNTHETIC ClassOption whose field 0 holds the
    // ordinal. Under `--real-jdk` the varargs array carries genuine JDK enum
    // constants instead, and `java.lang.Enum` declares `name` before
    // `ordinal`, so field 0 is a String. Reading it as an `i32` silently
    // answered "not a nestmate" for NESTMATE too — see
    // `class_option_is_nestmate` for why that stayed invisible.

    /// Build a `ClassOption` shaped like a REAL JDK enum constant: slot 0
    /// carries `Enum.name` (a String), NOT the ordinal. `set_field_by_name`
    /// additionally places the name wherever the mock's field model says
    /// `name` lives, and slot 0 is deliberately a String so the synthetic
    /// ordinal fallback CANNOT be what answers.
    fn make_real_enum_class_option(ctx: &mut MockNativeContext, const_name: &str) -> ObjectRef {
        let opt_cid = ctx
            .ensure_class_initialized("java/lang/invoke/MethodHandles$Lookup$ClassOption")
            .expect("alloc class option cid");
        let opt = ctx.alloc_object(opt_cid, 4);
        let name_obj = ctx.create_string(const_name);
        ctx.set_field(opt, 0, Value::Object(Some(name_obj)));
        ctx.set_field_by_name(opt, "name", Value::Object(Some(name_obj)));
        opt
    }

    /// As `drive_hidden_define`, but the ClassOption[] elements use the
    /// real-JDK enum layout. Returns the captured `nest_host_class_name`.
    fn drive_hidden_define_real_enum_options(
        lookup_class_name: &str,
        const_names: &[&str],
    ) -> Option<String> {
        let mut ctx = MockNativeContext::new();
        let (lookup, _lookup_cid) = make_lookup_for(&mut ctx, lookup_class_name);

        let class_bytes = cafebabe_minimal();
        let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
        }

        let opts_arr = ctx.new_array(
            cratonvm_types::ArrayElementType::Reference,
            const_names.len(),
        );
        for (i, n) in const_names.iter().enumerate() {
            let opt = make_real_enum_class_option(&mut ctx, n);
            ctx.set_array_element(opts_arr, i, Value::Object(Some(opt)));
        }

        let opts = no_options(&mut ctx);
        let r = lk_define_hidden_class_full(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes_arr)),
                Value::Int(0), // initialize = false
                Value::Object(Some(opts_arr)),
            ],
        );
        assert!(r.is_ok(), "defineHiddenClass must succeed: {:?}", r.err());
        ctx.last_define_full_opts()
            .expect("define_class_full was not invoked")
            .nest_host_class_name
    }

    /// THE REAL-JDK NESTMATE ARM. FAILS BEFORE THE W3-2 FIX.
    ///
    /// `RJdkHidden.java:101` appeared to pass only because the *class file's*
    /// `NestHost` attribute happened to name the same class the NESTMATE
    /// option would have selected. Assert the option itself is decoded, so
    /// the answer no longer depends on that coincidence.
    #[test]
    fn real_jdk_enum_class_option_is_decoded_as_nestmate() {
        let nh = drive_hidden_define_real_enum_options("RJdkHidden", &["NESTMATE"]);
        assert_eq!(
            nh.as_deref(),
            Some("RJdkHidden"),
            "a real-JDK ClassOption enum constant stores its NAME in slot 0; \
             NESTMATE must still be recognised"
        );
    }

    /// Control: the real-JDK layout must not turn STRONG into a nestmate.
    /// `Enum.ordinal` for STRONG is 1 and its `flag` is 0x4 — neither may be
    /// mistaken for the NESTMATE bit.
    #[test]
    fn real_jdk_enum_strong_only_is_not_a_nestmate() {
        let nh = drive_hidden_define_real_enum_options("RJdkHidden", &["STRONG"]);
        assert_eq!(
            nh, None,
            "STRONG alone must leave the hidden class in its own nest"
        );
    }

    /// The Weld/LambdaMetafactory pattern under the real-JDK layout: order
    /// must not matter, and STRONG must not veto NESTMATE.
    #[test]
    fn real_jdk_enum_strong_then_nestmate_still_propagates() {
        let nh = drive_hidden_define_real_enum_options("RJdkHidden", &["STRONG", "NESTMATE"]);
        assert_eq!(nh.as_deref(), Some("RJdkHidden"));
    }

    /// Defence-in-depth: the WithClassData variant (used by
    /// LambdaMetafactory and Weld's classData-bound proxies) also walks
    /// the ClassOption[] correctly. We only need a single happy-path
    /// assertion since the helper functions are shared with the plain
    /// variant.
    #[test]
    fn with_class_data_also_propagates_nestmate() {
        let mut ctx = MockNativeContext::new();
        let (lookup, lookup_cid) =
            make_lookup_for(&mut ctx, "weld/cdi/BeanManagerImpl$BeanFactory");
        ctx.set_nest_host_override(lookup_cid, "weld/cdi/BeanManagerImpl");

        let class_bytes = cafebabe_minimal();
        let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
        }
        let payload = ctx.alloc_object(ClassId::new(1), 1);
        let opts_arr = make_options_array(&mut ctx, &[0, 1]);

        let opts = no_options(&mut ctx);
        let r = lk_define_hidden_class_with_class_data(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes_arr)),
                Value::Object(Some(payload)),
                Value::Int(0),
                Value::Object(Some(opts_arr)),
            ],
        );
        assert!(r.is_ok(), "defineHiddenClassWithClassData must succeed");

        let captured = ctx
            .last_define_full_opts()
            .expect("define_class_full not invoked")
            .nest_host_class_name;
        assert_eq!(
            captured.as_deref(),
            Some("weld/cdi/BeanManagerImpl"),
            "WithClassData variant must also resolve to the outer nest host"
        );
    }

    // -----------------------------------------------------------------
    // W6-3 — the REAL-JDK `MethodHandles$Lookup` layout
    // -----------------------------------------------------------------

    /// `javap -p java.lang.invoke.MethodHandles$Lookup` on JDK 25, instance
    /// fields in declaration order:
    ///
    /// ```text
    ///   0 lookupClass            (ref)  private final Class<?>
    ///   1 prevLookupClass        (ref)  private final Class<?>
    ///   2 allowedModes           (int)  private final int
    ///   3 cachedProtectionDomain (ref)  private volatile ProtectionDomain
    /// ```
    ///
    /// The synthetic layout disagrees from slot 1 onwards
    /// (`lookupClass | allowedModes | previousLookupClass | lookupMode`), so
    /// writing modes BY INDEX type-confused all three: `Int(0x5F)` landed in the
    /// `prevLookupClass` REFERENCE slot, a null ref landed in `allowedModes` —
    /// which every JDK access check reads as 0, "no access at all" — and another
    /// `Int(0x5F)` landed in the `cachedProtectionDomain` reference slot. None of
    /// the three threw anything; the only symptom was a powerless Lookup.
    ///
    /// Declaring the real field names on the mock's class makes the by-name arm
    /// the one that runs, which is what this asserts.
    #[test]
    fn alloc_lookup_for_writes_real_jdk_layout_by_name() {
        use cratonvm_native_api::FieldMetadata;
        let mut ctx = MockNativeContext::new();
        let lk_cid = ctx.ensure_class_initialized(LK_CLASS).expect("lookup cid");
        let fm = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: lk_cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            lk_cid,
            vec![
                fm("lookupClass", "Ljava/lang/Class;", 0),
                fm("prevLookupClass", "Ljava/lang/Class;", 1),
                fm("allowedModes", "I", 2),
                fm(
                    "cachedProtectionDomain",
                    "Ljava/security/ProtectionDomain;",
                    3,
                ),
            ],
        );

        let host_cid = ctx.ensure_class_initialized("p/Host").expect("host cid");
        let mirror = ctx.get_class_mirror(host_cid);
        let lookup = alloc_lookup_for(&mut ctx, mirror).unwrap();

        assert!(
            matches!(ctx.get_field(lookup, 0), Value::Object(Some(m)) if m == mirror),
            "lookupClass is slot 0 in BOTH layouts"
        );
        assert_ne!(
            ctx.get_field(lookup, 1),
            Value::Int(0x5F),
            "slot 1 is the `prevLookupClass` REFERENCE — the mode word must not land here"
        );
        assert_eq!(
            ctx.get_field(lookup, 2),
            Value::Int(0x5F),
            "allowedModes must be FULL_POWER_MODES (95 — measured on JDK 25 as \
             `MethodHandles.lookup().lookupModes()`); a null here reads back as 0, \
             which is `no access at all`"
        );
        assert_ne!(
            ctx.get_field(lookup, 3),
            Value::Int(0x5F),
            "cachedProtectionDomain is a lazy volatile REFERENCE cache; it must stay \
             null rather than receive an integer the GC would scan as an oop"
        );
    }

    /// The negative half: with NO real field names declared, the object is a
    /// fabricated stub (`_f0.._f3`), the by-name lookups all miss, and the
    /// synthetic indices must still be written — `allowedModes` at slot 1.
    /// A by-name-only fix would have left this arm powerless.
    ///
    /// This test was GREEN while production took the opposite arm. The mock's
    /// `get_field_by_name` answers `Int(0)` for an unresolvable name;
    /// production answers `Value::Object(None)`, which the discriminator's old
    /// `matches!(.., Value::Object(_))` disjunct accepted as proof of the real
    /// layout. The discriminator is now the class-side witness alone, which
    /// both contexts answer identically, so this assertion means in production
    /// what it means here.
    #[test]
    fn alloc_lookup_for_still_writes_the_synthetic_indices() {
        let mut ctx = MockNativeContext::new();
        let host_cid = ctx.ensure_class_initialized("p/Host").expect("host cid");
        let mirror = ctx.get_class_mirror(host_cid);
        let lookup = alloc_lookup_for(&mut ctx, mirror).unwrap();
        assert!(
            matches!(ctx.get_field(lookup, 0), Value::Object(Some(m)) if m == mirror),
            "synthetic slot 0 is lookupClass"
        );
        assert_eq!(
            ctx.get_field(lookup, 1),
            Value::Int(0x5F),
            "synthetic slot 1 is allowedModes"
        );
        assert_eq!(
            ctx.get_field(lookup, 2),
            Value::Object(None),
            "synthetic slot 2 is previousLookupClass"
        );
    }

    /// Smoke: keep the unused `dummy_this` helper alive so editors don't
    /// flag it; future negative tests may want a null-this lookup.
    #[test]
    fn dummy_this_is_null_object() {
        assert!(matches!(dummy_this(), Value::Object(None)));
    }

    // -----------------------------------------------------------------
    // CGLIB-η — Loader inheritance tests
    // -----------------------------------------------------------------
    //
    // CGLIB 3.4+ / Spring 5.x emit proxies via `Lookup.defineClass` and
    // `Lookup.defineHiddenClass`. After defining, CGLIB does
    // `Class.forName(name, false, hostLoader)` to round-trip back to the
    // mirror — which only works if the new class lives in the lookup
    // class's loader namespace, NOT the application loader. Prior to the
    // CGLIB-η fix, both natives passed `loader_id = 0` (Application)
    // unconditionally, so any class defined via a Lookup whose host was
    // in a user-defined loader (Spring Boot's LaunchedURLClassLoader,
    // OSGi bundle loaders, web-app WAR loaders) would be lost.
    //
    // These tests pin that the lookup class's loader is inherited by:
    //   1. `Lookup.defineClass(byte[])` (the normal-class variant);
    //   2. `Lookup.defineHiddenClass(...)`;
    //   3. `Lookup.defineHiddenClassWithClassData(...)`.
    //
    // And that built-in loaders (Bootstrap/Extension/Application) all
    // still collapse to `loader_id = 0` so we don't regress the common
    // case.

    /// Acceptance #1: a user-defined loader (id >= 3) on the lookup class
    /// must be inherited by `Lookup.defineClass`.
    #[test]
    fn define_class_inherits_user_defined_loader() {
        let mut ctx = MockNativeContext::new();
        let (lookup, lookup_cid) = make_lookup_for(&mut ctx, "spring/boot/Service");
        // Spring Boot's LaunchedURLClassLoader gets a user-defined id.
        ctx.set_loader_id_override(lookup_cid, 7);

        let class_bytes = cafebabe_minimal();
        let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
        }
        let r = lk_define_class_b(
            &mut ctx,
            &[Value::Object(Some(lookup)), Value::Object(Some(bytes_arr))],
        );
        assert!(r.is_ok(), "defineClass must succeed: {:?}", r.err());
        assert_eq!(
            ctx.last_define_full_loader(),
            Some(7),
            "Lookup.defineClass must inherit the lookup class's user-defined loader"
        );
        let opts = ctx
            .last_define_full_opts()
            .expect("Lookup.defineClass must pass full definition options");
        assert!(
            opts.force_loader_faithful_linking,
            "generated classes must request loader-faithful linking"
        );
        assert!(
            opts.superclass_id_override.is_some(),
            "the superclass resolved through the lookup class must be preserved by identity"
        );
        assert!(
            opts.interface_id_overrides.is_some(),
            "the complete interface identity list must be preserved, including an empty list"
        );
    }

    /// Acceptance #2: Application loader (raw id 2) collapses to backend
    /// loader_id 0. Same for Bootstrap (0) and Extension (1) — the
    /// backend has no separate Bootstrap/Extension namespace today, so
    /// they all share the Application table.
    #[test]
    fn define_class_built_in_loaders_collapse_to_zero() {
        for raw in [0_i32, 1, 2] {
            let mut ctx = MockNativeContext::new();
            let (lookup, lookup_cid) = make_lookup_for(&mut ctx, "java/util/HashMap");
            ctx.set_loader_id_override(lookup_cid, raw);

            let class_bytes = cafebabe_minimal();
            let bytes_arr =
                ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
            for (i, b) in class_bytes.iter().enumerate() {
                ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
            }
            let r = lk_define_class_b(
                &mut ctx,
                &[Value::Object(Some(lookup)), Value::Object(Some(bytes_arr))],
            );
            assert!(r.is_ok(), "defineClass must succeed for raw={raw}");
            assert_eq!(
                ctx.last_define_full_loader(),
                Some(0),
                "raw loader {raw} (built-in) must collapse to backend loader_id 0"
            );
        }
    }

    /// Acceptance #3: `Lookup.defineHiddenClass(...)` inherits the
    /// user-defined loader. CGLIB 3.4+ relies on this so the hidden
    /// proxy is loader-visible to its host.
    #[test]
    fn define_hidden_class_inherits_user_defined_loader() {
        let mut ctx = MockNativeContext::new();
        let (lookup, lookup_cid) = make_lookup_for(&mut ctx, "cglib/Enhancer");
        ctx.set_loader_id_override(lookup_cid, 11);

        let class_bytes = cafebabe_minimal();
        let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
        }
        let opts_arr = make_options_array(&mut ctx, &[0]); // NESTMATE
        let opts = no_options(&mut ctx);
        let r = lk_define_hidden_class_full(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes_arr)),
                Value::Int(0),
                Value::Object(Some(opts_arr)),
            ],
        );
        assert!(r.is_ok(), "defineHiddenClass must succeed: {:?}", r.err());
        assert_eq!(
            ctx.last_define_full_loader(),
            Some(11),
            "Lookup.defineHiddenClass must inherit the lookup class's user-defined loader"
        );
        let opts = ctx
            .last_define_full_opts()
            .expect("defineHiddenClass must pass full definition options");
        assert!(opts.force_loader_faithful_linking);
        assert!(opts.superclass_id_override.is_some());
        assert!(opts.interface_id_overrides.is_some());
    }

    /// Acceptance #4: `Lookup.defineHiddenClassWithClassData(...)` also
    /// inherits the user-defined loader — LambdaMetafactory and CGLIB
    /// classData-bound proxies both rely on this.
    #[test]
    fn define_hidden_class_with_class_data_inherits_loader() {
        let mut ctx = MockNativeContext::new();
        let (lookup, lookup_cid) = make_lookup_for(&mut ctx, "lambda/Host");
        ctx.set_loader_id_override(lookup_cid, 42);
        let payload = ctx.alloc_object(ClassId::new(1), 1);

        let class_bytes = cafebabe_minimal();
        let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
        }
        let opts = no_options(&mut ctx);
        let r = lk_define_hidden_class_with_class_data(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(bytes_arr)),
                Value::Object(Some(payload)),
                Value::Int(0),
                Value::Object(Some(opts)),
            ],
        );
        assert!(r.is_ok(), "defineHiddenClassWithClassData must succeed");
        assert_eq!(
            ctx.last_define_full_loader(),
            Some(42),
            "Lookup.defineHiddenClassWithClassData must inherit the user-defined loader"
        );
        let opts = ctx
            .last_define_full_opts()
            .expect("defineHiddenClassWithClassData must pass full definition options");
        assert!(opts.force_loader_faithful_linking);
        assert!(opts.superclass_id_override.is_some());
        assert!(opts.interface_id_overrides.is_some());
    }
}
