// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `jdk.internal.misc.Unsafe` / `sun.misc.Unsafe` natives, the off-heap arena and the field-offset model.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use crate::lang_class::ClassTextName;
use super::*;
use crate::unsafe_natives::slot0_read_keep_alive;

#[inline]
pub(crate) fn unsafe_offset_is_heap_slot(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
) -> bool {
    // Craton heap field offsets are slot indexes. HotSpot/JCTools byte offsets
    // may be numerically below padded mirror field counts, so cap heap-slot
    // treatment to the small real-slot range.
    offset < ctx.object_num_fields(obj) && offset < 64
}

/// `CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC=0` (nio21 wave 7, lane L3) puts the
/// `Unsafe` natives back on their old arm order, where the synthetic-offset
/// band was tested before the array arm. Default ON. Read only on the rare path
/// where an ARRAY base meets an offset in the band (see
/// [`unsafe_synthetic_field_offset`]), so it is not cached. The JIT's RMW
/// prefixes read the same switch at compile time
/// (`cratonvm_jit::direct_helpers::unsafe_inline_rmw_screen_bytes`) and keep
/// their band decline while it is off, so the two tiers stay in one domain.
pub(crate) fn unsafe_array_before_synthetic_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC")
}

/// Does `offset` name a SYNTHETIC field slot for the non-null base `obj`, so
/// that the side store (`synthetic_get` / `synthetic_put` / `synthetic_cas`)
/// answers it?
///
/// nio21 wave 7 L3 (`docs/internal/fixed-bugs/r21-unsafe-natives-test-synthetic-offset-before-the-array-FIXED-20261010.md`).
/// A synthetic offset (`0x2000_0000..=0x3FFF_FFFF`) is the VM's stand-in for a
/// field it could not lay out, and it never means anything against an ARRAY
/// base. An in-range element offset of an array whose payload exceeds 512 MiB
/// does fall in that band, though, and the natives used to test the band first,
/// so such an element was read and written in a side store keyed by the array
/// instead of in the array. HotSpot addresses the element. So: an array base is
/// never synthetic, and its offset goes to the array arm that follows every
/// call site of this function (out of range there is the arm's own "no heap
/// touch" answer). A non-array base keeps the band.
#[inline]
pub(crate) fn unsafe_synthetic_field_offset(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
) -> bool {
    if !is_synthetic_offset(offset) {
        return false;
    }
    ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array
        || !unsafe_array_before_synthetic_enabled()
}

/// `Unsafe.pageSize()` — the host OS virtual-memory page size.
///
/// Was a hard-coded `4096`. That is right on x86-64 Linux/Windows but wrong on
/// an Apple-silicon host (16 KiB) and on Linux/aarch64 kernels built with 16 K
/// or 64 K pages, where callers that size a buffer from `pageSize()` (Netty's
/// `PlatformDependent`, LMAX Disruptor padding, `DirectByteBuffer` alignment)
/// would under-align. `sysconf(_SC_PAGESIZE)` is the same source HotSpot uses.
/// Windows has no `sysconf`; its page size is 4 KiB on every architecture the
/// VM builds for, so the constant stays there and is documented as such.
pub(crate) fn native_unsafe_page_size(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    #[cfg(unix)]
    let size: i32 = {
        // `sysconf` returns -1 on failure; fall back to the 4 KiB default.
        let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        i32::try_from(raw).ok().filter(|n| *n > 0).unwrap_or(4096)
    };
    #[cfg(not(unix))]
    let size: i32 = 4096;
    Ok(Some(Value::Int(size)))
}

/// `Unsafe.addressSize()` — the width of a native pointer in bytes.
///
/// Derived from the host pointer width rather than hard-coded to 8, so it
/// agrees with `jdk/internal/misc/Unsafe.addressSize0()` (unsafe_jdk25.rs) and
/// stays correct if the VM is ever built for a 32-bit target.
/// The value BOTH `addressSize()` and `Unsafe.ADDRESS_SIZE` answer with.
///
/// One constant because the two used to disagree by 8: the native below
/// answered the host's pointer size and the `public static final int
/// ADDRESS_SIZE` that the image's own bytecode reads was never assigned at all.
const UNSAFE_ADDRESS_SIZE: i32 = std::mem::size_of::<usize>() as i32;

pub(crate) fn native_unsafe_address_size(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(UNSAFE_ADDRESS_SIZE)))
}

pub(crate) fn native_unsafe_ensure_class_initialized(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let class_mirror = args.iter().find_map(|v| match v {
        Value::Object(Some(obj)) => {
            let obj_cid = ctx.class_id_of_object(*obj);
            let is_class_mirror = ctx
                .class_name_of_id(obj_cid)
                .map(|n| n == "java/lang/Class")
                .unwrap_or(false);
            if is_class_mirror && crate::lang_class::mirror_class_id(ctx, *obj).is_some() {
                Some(*obj)
            } else {
                None
            }
        }
        _ => None,
    });
    // MEASURED, HotSpot 25.0.4+7: `ensureClassInitialized(null)` is a
    // `NullPointerException`; CratonVM returned quietly. The mirror is found
    // by SCANNING the arguments (it is not at a fixed index on every call
    // path), so the null case can only be recognised by asking whether
    // argument 1 was explicitly null after the scan came up empty.
    if class_mirror.is_none() && matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some("Unsafe.ensureClassInitialized: null class".to_string()),
        }
        .into());
    }
    let Some(class_mirror) = class_mirror else {
        return Ok(None);
    };
    let Some(class_name) = crate::lang_class::mirror_class_name(ctx, class_mirror) else {
        return Ok(None);
    };
    if class_name.starts_with('[') {
        return Ok(None);
    }
    match class_name.as_str() {
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void" => {
            return Ok(None)
        }
        _ => {}
    }
    // Under `--jdk-only`, initialize THE class the mirror is, by its id. The
    // name named the application class path's copy (or nothing) for a class a
    // user loader defined, so `Lookup.ensureInitialized`, the
    // `DirectMethodHandle` init barrier, `MethodHandleAccessorFactory` and
    // `LazyInitializingVarHandle` -- every JDK route through
    // `Unsafe.ensureClassInitialized` -- ran another class's `<clinit>` and
    // left this one uninitialized, while `shouldBeInitialized0` (by id) kept
    // reporting it uninitialized (interpreter round i1 wave 29, lane L4;
    // `tools/probes/interp/L4/L4W29EnsureInitializedByMirror.java`).
    // `--compatible` keeps its by-name route.
    if ctx.is_jdk_only() {
        if let Some(class_id) = crate::lang_class::mirror_class_id(ctx, class_mirror) {
            ctx.ensure_class_initialized_with_class_id(class_id)?;
            return Ok(None);
        }
    }
    // Interpreter round i2 wave 60, lane L3: `--compatible` serves
    // `jdk.internal.misc.Unsafe.ensureClassInitialized` itself (the JDK's body
    // calls the `native` `ensureClassInitialized0`), so a failed `<clinit>`
    // listed neither frame (`L3W57ReflectiveClassInitFailure` /
    // `L3W58ReflectiveClassInit` under `CRATONVM_JDK_REFLECTIVE_INVOKE`).
    // An uninitialized class of a `jdk.internal.misc.Unsafe` receiver is
    // initialized under the two frames HotSpot raises it in.
    if let Some(class_id) = crate::lang_class::mirror_class_id(ctx, class_mirror) {
        if !ctx.is_class_initialized(class_id) && receiver_is_internal_unsafe(ctx, args.first()) {
            return ensure_class_initialized_served(ctx, class_id).map(|()| None);
        }
    }
    ctx.ensure_class_initialized(&class_name)?;
    Ok(None)
}

/// Is `receiver` a `jdk.internal.misc.Unsafe` (not `sun.misc.Unsafe`, whose
/// own frame HotSpot would list above)?
fn receiver_is_internal_unsafe(ctx: &dyn NativeContext, receiver: Option<&Value>) -> bool {
    match receiver {
        Some(Value::Object(Some(r))) => {
            ctx.class_name_of_id(ctx.class_id_of_object(*r)).as_deref()
                == Some("jdk/internal/misc/Unsafe")
        }
        _ => false,
    }
}

/// `jdk.internal.misc.Unsafe.ensureClassInitialized`'s initialization of
/// `class_id`, its failure listed as HotSpot raises it: inside
/// `Unsafe.ensureClassInitialized0 (Native Method)`, under
/// `Unsafe.ensureClassInitialized` at its call (measured, JDK 25.0.3,
/// `L3W58ReflectiveClassInit` in `--jdk-only`, where the JDK body runs).
/// Interpreter round i2 wave 60, lane L3.
#[cold]
#[inline(never)]
fn ensure_class_initialized_served(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const UNSAFE: &str = "jdk/internal/misc/Unsafe";
    const DESC: &str = "(Ljava/lang/Class;)V";
    ctx.initialize_class_served(
        class_id,
        ServedRaiseChain {
            throwable_class: "java/lang/ExceptionInInitializerError",
            hops: vec![
                ServedRaiseHop {
                    class: UNSAFE,
                    name: "ensureClassInitialized",
                    descriptor: DESC,
                    site: ServedRaiseHopSite::LastCall {
                        name: "ensureClassInitialized0",
                        descriptor: Some(DESC),
                    },
                },
                ServedRaiseHop {
                    class: UNSAFE,
                    name: "ensureClassInitialized0",
                    descriptor: DESC,
                    site: ServedRaiseHopSite::Raise,
                },
            ],
        },
    )
}

/// `Unsafe.shouldBeInitialized(Class)` / `shouldBeInitialized0(Class)` —
/// "does this class still need its `<clinit>` run?".
///
/// HotSpot answers `!k->is_initialized()`, so this reads the VM's own per-class
/// init state through [`NativeContext::is_class_initialized`] rather than
/// asserting a constant. Both natives are instance methods on `Unsafe`, so the
/// mirror is not at a fixed argument index on every call path; scan for it the
/// same way [`native_unsafe_ensure_class_initialized`] does.
///
/// Non-mirror arguments, primitive mirrors and array mirrors all answer
/// `false`: none of them has a `<clinit>` that could still be pending.
pub(crate) fn native_unsafe_should_be_initialized(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let class_id = args.iter().find_map(|v| match v {
        Value::Object(Some(obj)) => {
            let obj_cid = ctx.class_id_of_object(*obj);
            let is_class_mirror = ctx
                .class_name_of_id(obj_cid)
                .map(|n| n == "java/lang/Class")
                .unwrap_or(false);
            if is_class_mirror {
                crate::lang_class::mirror_class_id(ctx, *obj)
            } else {
                None
            }
        }
        _ => None,
    });
    let Some(class_id) = class_id else {
        return Ok(Some(Value::Int(0)));
    };
    if let Some(name) = ctx.class_name_of_id(class_id) {
        if name.starts_with('[')
            || matches!(
                name.as_str(),
                "boolean"
                    | "byte"
                    | "char"
                    | "short"
                    | "int"
                    | "long"
                    | "float"
                    | "double"
                    | "void"
            )
        {
            return Ok(Some(Value::Int(0)));
        }
    }
    let initialized = ctx.is_class_initialized(class_id);
    Ok(Some(Value::Int(if initialized { 0 } else { 1 })))
}

/// `jdk.internal.misc.CDS.isSharingEnabled0()` — is a class-data-sharing
/// archive actually mapped into this run?
///
/// `vm/src/vm/vm_init.rs` sets `jdk.internal.vm.cds.enabled` when
/// `-Xshare:on|auto` successfully loads the archive named by
/// `-XX:SharedArchiveFile`. Same source as
/// `cds.rs::native_cds_is_sharing_enabled`, so the `0`-suffixed native (the
/// one the real JDK's `CDS.<clinit>` calls) and the synthetic-mode
/// `isSharingEnabled` now agree.
fn native_cds_is_sharing_enabled0(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let enabled = ctx
        .get_system_property("jdk.internal.vm.cds.enabled")
        .is_some_and(|v| v == "true");
    Ok(Some(Value::Int(i32::from(enabled))))
}

/// `jdk.internal.misc.CDS.isDumpingArchive0()` — will this run produce an
/// archive? True under `-Xshare:dump`, which makes `SharedVm` write the
/// archive at shutdown; `vm_init.rs` publishes that as
/// `jdk.internal.vm.cds.dumping`.
fn native_cds_is_dumping_archive0(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let dumping = ctx
        .get_system_property("jdk.internal.vm.cds.dumping")
        .is_some_and(|v| v == "true");
    Ok(Some(Value::Int(i32::from(dumping))))
}

// ---------------------------------------------------------------------------
// Phase 14 Step 3: String extras
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Step 5: Math natives
// ---------------------------------------------------------------------------

// --- abs ---
// --- max ---
// --- min ---
// --- trig and math functions ---
// ---------------------------------------------------------------------------
// Phase 13 Step 1: Math exact arithmetic
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Phase 13 Step 2: Floor/ceil division + toIntExact
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Phase 13 Step 3: Advanced math functions
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Step 6: Wrapper type boxing/unboxing
// ---------------------------------------------------------------------------

/// Helper: allocate a wrapper object with 1 field using a well-known class name.
/// Falls back to ClassId(0) with 1 field if the class can't be loaded.
/// Shared unboxing for Integer.intValue(), Boolean.booleanValue(),
/// Character.charValue(), Byte.byteValue(), Short.shortValue().
// --- Long ---

// --- Boolean ---

// --- Character ---

// ---------------------------------------------------------------------------
// Phase 12: Number wrapper cross-type conversion helpers
// ---------------------------------------------------------------------------

// Int-stored field 0 → Long
// Int-stored field 0 → Float
// Int-stored field 0 → Double
// Long field 0 → Int
// Long field 0 → Float
// Long field 0 → Double
// Float field 0 → Int
// Float field 0 → Long
// Float field 0 → Double
// Double field 0 → Int
// Double field 0 → Long
// Double field 0 → Float
// ---------------------------------------------------------------------------
// Phase 12: Wrapper instance toString / hashCode / equals
// ---------------------------------------------------------------------------

// toString for Int-stored wrappers (Integer, Byte, Short)
// toString for Long wrapper
// toString for Float wrapper
// toString for Double wrapper
// toString for Boolean wrapper
// toString for Character wrapper
// hashCode for Int-stored wrappers (Integer, Byte, Short, Character)
// hashCode for Boolean wrapper (true=1231, false=1237)
// hashCode for Long wrapper: (v ^ (v >>> 32)) as i32
// hashCode for Float wrapper: floatToIntBits(v)
// hashCode for Double wrapper: bits = doubleToLongBits; (bits ^ (bits >>> 32)) as i32
// equals for Int-stored wrappers (Integer, Boolean, Character, Byte, Short)
// equals for Long wrapper
// equals for Float wrapper (NaN == NaN is true per Float.equals spec, using to_bits)
// equals for Double wrapper (NaN == NaN is true per Double.equals spec, using to_bits)
// ---------------------------------------------------------------------------
// Phase 12: Character additional methods
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Phase 12: Boolean additional methods
// ---------------------------------------------------------------------------

// --- Integer/Long radix helpers ---

/// Convert a non-negative i64 to a string in the given radix.
// --- String.format (basic %s/%d/%f support) ---

/// Format a single argument with flags, width, and precision support.
#[allow(clippy::too_many_arguments)]
/// Extract a float value from a Value (unboxing wrappers as needed).
/// Format a single argument for String.format.
// --- Float ---

// --- Double (boxing) ---

// --- Float/Double parsing and utilities (Phase 8 Part 7) ---

// --- Byte ---

// --- Short ---

// ---------------------------------------------------------------------------
// Step 7: Enhanced Class support
// ---------------------------------------------------------------------------

/// Helper: read class_id from a Class mirror's field 0.
/// Helper: read class name from a Class mirror's field 1 (String).
// ---------------------------------------------------------------------------
// Test harness: tempPrint
// ---------------------------------------------------------------------------
// Reflection helpers
// ---------------------------------------------------------------------------

/// Convert a JVM type descriptor to a Class mirror object.
///
/// Handles primitives ("I" → int.class), object types ("Ljava/lang/String;" → String.class),
/// array types ("[I" → int[].class), and void ("V" → void.class).
/// Parse a method descriptor into (parameter type descriptors, return type descriptor).
///
/// e.g. "(ILjava/lang/String;)V" → (["I", "Ljava/lang/String;"], "V")
/// Box a VM Value into a wrapper object for reflection returns.
///
/// e.g. Value::Int(42) with type "I" → Integer.valueOf(42) object
/// Unbox a wrapper object to a primitive Value.
///
/// e.g. Integer object → Value::Int(42)
/// Unbox a Value::Object to a primitive based on the expected descriptor.
///
/// If the value is Object(None) (null), returns a default for the type.
/// If the value is already the right primitive type, returns it as-is.
// ---------------------------------------------------------------------------
// java.lang.reflect.Field — object layout and natives
// ---------------------------------------------------------------------------

/// Number of heap fields in a synthetic Field object.
// Field layout:
//   0 → Class mirror (declaring class)
//   1 → String (field name)
//   2 → Class mirror (field type, derived from descriptor)
//   3 → Int (modifiers = access_flags as i32)
//   4 → Int (slot_index — absolute heap index for instance, field_index for static)
//   5 → String (raw descriptor, e.g. "I" or "Ljava/lang/String;")

/// Create a Field reflection object from metadata.
/// Helper: read the static-flag, declaring ClassId, slot_index, and descriptor
/// from a Field reflection object's fields.
// --- Field getters (simple field reads) ---

// --- Field.get(Object) / Field.set(Object, Object) ---

// --- Typed Field getters (getInt, getLong, getFloat, getDouble, getBoolean) ---

// --- Typed Field setters (setInt, setLong, setFloat, setDouble, setBoolean) ---

// ---------------------------------------------------------------------------
// Class.getDeclaredFields / getDeclaredField
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// java.lang.reflect.Method — object layout and natives
// ---------------------------------------------------------------------------

/// Number of heap fields in a synthetic Method object.
// Method layout:
//   0 → Class mirror (declaring class)
//   1 → String (method name)
//   2 → Class mirror (return type)
//   3 → Object[] (Class mirrors for parameter types)
//   4 → Int (modifiers = access_flags as i32)
//   5 → String (raw descriptor, e.g. "(II)I")
//   6 → Int (parameter count)

/// Create a Method reflection object from metadata.
// --- Method getters ---

// --- Method.invoke ---

// ---------------------------------------------------------------------------
// Class.getDeclaredMethods / getDeclaredMethod
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// java.lang.reflect.Constructor — object layout and natives
// ---------------------------------------------------------------------------

/// Number of heap fields in a synthetic Constructor object.
// Constructor layout:
//   0 → Class mirror (declaring class)
//   1 → Object[] (Class mirrors for parameter types)
//   2 → Int (modifiers = access_flags as i32)
//   3 → String (raw descriptor, e.g. "(I)V")
//   4 → Int (parameter count)

/// Create a Constructor reflection object from method metadata (must be an <init>).
// --- Constructor getters ---

// --- Constructor.newInstance ---

// ---------------------------------------------------------------------------
// Class.getDeclaredConstructors / getDeclaredConstructor
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Inherited enumeration — getFields/getMethods/getConstructors
// ---------------------------------------------------------------------------

/// Collect all public fields from the class hierarchy (this class + superclasses + interfaces).
/// Collect all public methods from the class hierarchy.
// --- Class.getInterfaces / Class.getModifiers ---

// ---------------------------------------------------------------------------
// Annotation support (Phase 20)
// ---------------------------------------------------------------------------

/// Annotation proxy: 2-field synthetic object
///   field 0 = String (annotation type descriptor, e.g. "Ljava/lang/Override;")
///   field 1 = Class mirror (annotation type class, or null)

/// Convert an annotation type descriptor to an internal class name.
/// E.g. "Ljava/lang/Override;" -> "java/lang/Override"
/// Create an annotation proxy object from annotation data.
/// Build an Annotation[] array from annotation data.
/// Class.getAnnotations() / Class.getDeclaredAnnotations()
/// Class.getAnnotation(Class) / Class.getDeclaredAnnotation(Class)
/// Class.isAnnotationPresent(Class)
/// Class.isAnnotation() — checks if the class itself is an annotation type
/// Class.getAnnotationsByType(Class) / getDeclaredAnnotationsByType(Class)
/// Helper: extract declaring class ID and field name from a Field reflection object.
/// Helper: extract declaring class ID, method name, and descriptor from a Method reflection object.
/// Field.getAnnotations() / Field.getDeclaredAnnotations()
/// Field.isAnnotationPresent(Class)
/// Method.getAnnotations() / Method.getDeclaredAnnotations()
/// Method.isAnnotationPresent(Class)
/// Method.getAnnotation(Class)
/// Annotation.annotationType() — returns the Class mirror of the annotation type
// --- java.lang.reflect.Modifier ---

// ---------------------------------------------------------------------------

// ===========================================================================
// sun.misc.Unsafe
// ===========================================================================

pub(crate) fn register_unsafe_natives(r: &mut NativeMethodRegistry) {
    // census-tag: sun.misc.Unsafe — VM-internal memory/CAS primitives → Bridge.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let u = "sun/misc/Unsafe";
    // `registerNatives` only asks the VM to bind this class's JNI entry points.
    // CratonVM binds them at registry-build time, so there is nothing left to
    // do — an empty body is what HotSpot's own `Unsafe_RegisterNatives` amounts
    // to once the table is already populated.
    r.register(u, "<clinit>", "()V", |ctx, _args| {
        let class_name = "sun/misc/Unsafe";
        let unsafe_obj = try_alloc_concurrent_synthetic(ctx, class_name, 0)?;
        ctx.set_static_field_by_name(class_name, "theUnsafe", Value::Object(Some(unsafe_obj)));
        // `theInternalUnsafe` (the real `<clinit>` seeds it with
        // `jdk.internal.misc.Unsafe.getUnsafe()`) must ALSO be populated by
        // this shadow: every sun.misc.Unsafe method that is NOT natively
        // registered (putOrderedLong/putOrderedInt/putOrderedObject, ...)
        // runs its real bytecode, which is just
        // `theInternalUnsafe.putLongRelease(...)` etc. The interpreter
        // papered over the null via the C11/C16 null-receiver Unsafe rescue
        // (interpreter.rs invokevirtual: dispatch on the constant-pool class
        // when the receiver is null and the class is an Unsafe), but
        // JIT-compiled bytecode has no such rescue — its receiver null-check
        // throws NullPointerException as soon as the method tiers up.
        // Surfaced by netty's shaded jctools
        // `ConcurrentSequencedCircularArrayQueue.<init>` init loop
        // (`UnsafeLongArrayAccess.soLongElement` -> `Unsafe.putOrderedLong`):
        // a capacity >= ~1000 crosses the JIT threshold mid-loop, so
        // `MpmcArrayQueue(4096)` NPE'd while `MpmcArrayQueue(8)` worked,
        // poisoning `ByteBufUtil.<clinit>` and with it the whole
        // RSocket / Reactor-Netty reactive test cluster.
        if ctx
            .ensure_class_initialized("jdk/internal/misc/Unsafe")
            .is_ok()
        {
            if let Some(cid) = ctx.class_id_by_name("jdk/internal/misc/Unsafe") {
                if let Some(idx) = ctx.static_field_index_by_name(cid, "theUnsafe") {
                    let v = ctx.get_static_field(cid, idx);
                    if matches!(v, Value::Object(Some(_))) {
                        ctx.set_static_field_by_name(class_name, "theInternalUnsafe", v);
                    }
                }
            }
        }
        // The real `<clinit>` also does
        // `Reflection.registerMethodsToFilter(Unsafe.class, Set.of("getUnsafe"))`
        // (see `sun.misc.Unsafe`'s own static initializer, JDK 25) — the door
        // that hides reflective acquisition of the singleton behind the
        // `theUnsafe` FIELD instead. This shadow replaces the whole `<clinit>`
        // method body, so that real-bytecode call never runs unless
        // reproduced here (see `reflect_filter.rs`: every other filter-map
        // entry is read live off classes whose `<clinit>` runs as real
        // bytecode — Unsafe's is the one class in the probe whose `<clinit>`
        // this crate itself shadows).
        if let Some(unsafe_cid) = ctx.class_id_by_name(class_name) {
            let mirror = ctx.get_class_mirror(unsafe_cid);
            let name_str = ctx.create_string("getUnsafe");
            if let Ok(Some(Value::Object(Some(set)))) = ctx.invoke(
                "java/util/Set",
                "of",
                "(Ljava/lang/Object;)Ljava/util/Set;",
                &[Value::Object(Some(name_str))],
            ) {
                let mirror_pin = ctx.pin_native_root(mirror);
                let set_pin = ctx.pin_native_root(set);
                let mirror = ctx.read_native_pin(mirror_pin, mirror);
                let set = ctx.read_native_pin(set_pin, set);
                let _ = ctx.invoke(
                    "jdk/internal/reflect/Reflection",
                    "registerMethodsToFilter",
                    "(Ljava/lang/Class;Ljava/util/Set;)V",
                    &[Value::Object(Some(mirror)), Value::Object(Some(set))],
                );
                ctx.unpin_native_roots(mirror_pin);
            }
        }
        Ok(None)
    });
    r.register(u, "getUnsafe", "()Lsun/misc/Unsafe;", |ctx, _args| {
        let class_name = "sun/misc/Unsafe";
        // `getUnsafe()` is `@CallerSensitive`: the JDK hands the singleton only
        // to a caller on the boot/platform class path and throws
        // `SecurityException` otherwise. MEASURED, HotSpot 25.0.4+7: an
        // application-loader caller gets `SecurityException`; CratonVM handed
        // over `theUnsafe`. That is the difference between "reflection on
        // `theUnsafe` is the documented back door" and "there is no door to
        // close" -- and the reason every real-world snippet reaches for the
        // field rather than this method.
        if !unsafe_caller_is_boot_path(ctx) {
            return Err(RuntimeError::SecurityException {
                message: "Unsafe".to_string(),
            }
            .into());
        }
        ctx.ensure_class_initialized(class_name)?;
        let class_id = match ctx.class_id_by_name(class_name) {
            Some(id) => id,
            None => {
                return Ok(Some(Value::Object(Some(try_alloc_concurrent_synthetic(
                    ctx, class_name, 0,
                )?))))
            }
        };
        let value = match ctx.static_field_index_by_name(class_id, "theUnsafe") {
            Some(idx) => ctx.get_static_field(class_id, idx),
            None => Value::Object(Some(try_alloc_concurrent_synthetic(ctx, class_name, 0)?)),
        };
        Ok(Some(value))
    });
    r.register(
        u,
        "objectFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
        native_unsafe_object_field_offset,
    );
    r.register(
        u,
        "staticFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
        native_unsafe_static_field_offset,
    );
    r.register(
        u,
        "arrayBaseOffset",
        "(Ljava/lang/Class;)I",
        native_unsafe_array_base_offset,
    );
    r.register(
        u,
        "arrayIndexScale",
        "(Ljava/lang/Class;)I",
        native_unsafe_array_index_scale,
    );
    // CAS
    r.register(
        u,
        "compareAndSwapInt",
        "(Ljava/lang/Object;JII)Z",
        native_unsafe_cas_int,
    );
    r.register(
        u,
        "compareAndSwapLong",
        "(Ljava/lang/Object;JJJ)Z",
        native_unsafe_cas_long,
    );
    r.register(
        u,
        "compareAndSwapObject",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_cas_object,
    );
    // Volatile get/put
    r.register(
        u,
        "getIntVolatile",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_volatile_arena,
    );
    r.register(
        u,
        "putIntVolatile",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile_arena,
    );
    r.register(
        u,
        "getLongVolatile",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_volatile_arena,
    );
    r.register(
        u,
        "putLongVolatile",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_volatile_arena,
    );
    r.register(
        u,
        "getObjectVolatile",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object_volatile,
    );
    r.register(
        u,
        "putObjectVolatile",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object_volatile,
    );
    // Non-volatile get/put
    r.register(
        u,
        "getObject",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object,
    );
    r.register(
        u,
        "putObject",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object,
    );
    r.register(
        u,
        "getInt",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_mb,
    );
    r.register(
        u,
        "putInt",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_mb,
    );
    r.register(
        u,
        "getLong",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_mb,
    );
    r.register(
        u,
        "putLong",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_mb,
    );
    // Allocation
    r.register(
        u,
        "allocateInstance",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        |ctx, args| {
            // Interpreter round i2 wave 63, lane L1: JVMTI `VMObjectAlloc`
            // (a reflective constructor's `DirectMethodHandle.allocateInstance`).
            let made = native_unsafe_allocate_instance(ctx, args);
            crate::vm_object_alloc_event(ctx, made)
        },
    );
    // Fences
    r.register(u, "storeFence", "()V", native_unsafe_fence);
    r.register(u, "loadFence", "()V", native_unsafe_fence);
    r.register(u, "fullFence", "()V", native_unsafe_fence);
    // Park/Unpark
    r.register(u, "park", "(ZJ)V", native_unsafe_park);
    r.register(u, "unpark", "(Ljava/lang/Object;)V", native_unsafe_unpark);
    // Compound ops
    r.register(
        u,
        "getAndAddInt",
        "(Ljava/lang/Object;JI)I",
        native_unsafe_get_and_add_int,
    );
    r.register(
        u,
        "getAndSetInt",
        "(Ljava/lang/Object;JI)I",
        native_unsafe_get_and_set_int,
    );
    // getAndAddLong / getAndSetLong — needed by AtomicLong
    r.register(
        u,
        "getAndAddLong",
        "(Ljava/lang/Object;JJ)J",
        native_unsafe_get_and_add_long,
    );
    r.register(
        u,
        "getAndSetLong",
        "(Ljava/lang/Object;JJ)J",
        native_unsafe_get_and_set_long,
    );
    r.register(
        u,
        "getAndSetObject",
        "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
        native_unsafe_get_and_set_object,
    );
    // Primitive field access (boolean, byte, short, float, double, char)
    r.register(
        u,
        "getBoolean",
        "(Ljava/lang/Object;J)Z",
        native_unsafe_get_boolean_mb,
    );
    r.register(
        u,
        "putBoolean",
        "(Ljava/lang/Object;JZ)V",
        native_unsafe_put_boolean_mb,
    );
    r.register(
        u,
        "getByte",
        "(Ljava/lang/Object;J)B",
        native_unsafe_get_byte_mb,
    );
    r.register(
        u,
        "putByte",
        "(Ljava/lang/Object;JB)V",
        native_unsafe_put_byte_mb,
    );
    r.register(
        u,
        "getShort",
        "(Ljava/lang/Object;J)S",
        native_unsafe_get_short_mb,
    );
    r.register(
        u,
        "putShort",
        "(Ljava/lang/Object;JS)V",
        native_unsafe_put_short_mb,
    );
    r.register(
        u,
        "getFloat",
        "(Ljava/lang/Object;J)F",
        native_unsafe_get_float_mb,
    );
    r.register(
        u,
        "putFloat",
        "(Ljava/lang/Object;JF)V",
        native_unsafe_put_float_mb,
    );
    r.register(
        u,
        "getDouble",
        "(Ljava/lang/Object;J)D",
        native_unsafe_get_double_mb,
    );
    r.register(
        u,
        "putDouble",
        "(Ljava/lang/Object;JD)V",
        native_unsafe_put_double_mb,
    );
    r.register(
        u,
        "getChar",
        "(Ljava/lang/Object;J)C",
        native_unsafe_get_char_mb,
    );
    r.register(
        u,
        "putChar",
        "(Ljava/lang/Object;JC)V",
        native_unsafe_put_char_mb,
    );
    // Volatile variants for primitive types
    r.register(
        u,
        "getBooleanVolatile",
        "(Ljava/lang/Object;J)Z",
        native_unsafe_get_boolean_volatile,
    );
    r.register(
        u,
        "putBooleanVolatile",
        "(Ljava/lang/Object;JZ)V",
        native_unsafe_put_boolean_volatile,
    );
    r.register(
        u,
        "getByteVolatile",
        "(Ljava/lang/Object;J)B",
        native_unsafe_get_byte_volatile,
    );
    r.register(
        u,
        "putByteVolatile",
        "(Ljava/lang/Object;JB)V",
        native_unsafe_put_byte_volatile,
    );
    r.register(
        u,
        "getShortVolatile",
        "(Ljava/lang/Object;J)S",
        native_unsafe_get_short_volatile,
    );
    r.register(
        u,
        "putShortVolatile",
        "(Ljava/lang/Object;JS)V",
        native_unsafe_put_short_volatile,
    );
    r.register(
        u,
        "getFloatVolatile",
        "(Ljava/lang/Object;J)F",
        native_unsafe_get_int_volatile,
    );
    r.register(
        u,
        "putFloatVolatile",
        "(Ljava/lang/Object;JF)V",
        native_unsafe_put_int_volatile,
    );
    r.register(
        u,
        "getDoubleVolatile",
        "(Ljava/lang/Object;J)D",
        native_unsafe_get_long_volatile,
    );
    r.register(
        u,
        "putDoubleVolatile",
        "(Ljava/lang/Object;JD)V",
        native_unsafe_put_long_volatile,
    );
    r.register(
        u,
        "getCharVolatile",
        "(Ljava/lang/Object;J)C",
        native_unsafe_get_char_volatile,
    );
    r.register(
        u,
        "putCharVolatile",
        "(Ljava/lang/Object;JC)V",
        native_unsafe_put_char_volatile,
    );
    // copyMemory — use the consolidated handler (superset of the heap↔heap
    // `native_unsafe_copy_memory`) so MIXED heap↔off-heap copies are NOT
    // dropped. This is the path `DirectByteBuffer.put/get(byte[])` takes via
    // `ScopedMemoryAccess.copyMemory`; with the bare heap↔heap handler the
    // off-heap side was silently lost, so a direct ByteBuffer used for NIO
    // socket I/O round-tripped as zeros (Tomcat http-nio never saw the
    // request/response bytes). See `unsafe_natives::native_unsafe_copy_memory_consolidated`.
    r.register(
        u,
        "copyMemory",
        "(Ljava/lang/Object;JLjava/lang/Object;JJ)V",
        unsafe_natives::native_unsafe_copy_memory_consolidated,
    );
    r.register(
        u,
        "setMemory",
        "(Ljava/lang/Object;JJB)V",
        native_unsafe_set_memory,
    );
    // pageSize / addressSize — queried from the host rather than hard-coded.
    r.register(u, "pageSize", "()I", native_unsafe_page_size);
    r.register(u, "addressSize", "()I", native_unsafe_address_size);

    // Also register under jdk/internal/misc/Unsafe (the modern API, same implementations)
    let u2 = "jdk/internal/misc/Unsafe";
    // Same as the `sun/misc/Unsafe` entry above: JNI binding is done at
    // registry-build time, so the method genuinely has no work to do.
    r.register_with_kind(
        u2,
        "registerNatives",
        "()V",
        native_noop,
        cratonvm_native_api::NativeKind::Bridge,
    );
    // This SHADOWS the image's own class initialiser, so every static the real
    // `<clinit>` assigns has to be assigned here or it stays at its default —
    // and a static that is never assigned is invisible until something reads it
    // through bytecode rather than through a native.
    //
    // `ADDRESS_SIZE` is exactly that. It is `public static final int` on all
    // three images and `addressSize()` is a one-instruction
    // `getstatic ADDRESS_SIZE` — so while the native below answered the
    // question the field read **0 here and 8 on HotSpot**, measured 2026-09-12,
    // and nothing could see it because every caller went through the native.
    // Retiring `addressSize()` in lane 5's third wave made the field the only
    // answer and turned the row red, which is how it surfaced; the row is out
    // of that wave and the defect is fixed here, because the two are not the
    // same thing.
    //
    // Seven other `getstatic ADDRESS_SIZE` sites in that class read it too —
    // `getAddress`/`putAddress` branch on `ADDRESS_SIZE == 4` — so a zero there
    // was one bytecode path away from picking the wrong width, and only
    // accidentally right because 0 is not 4.
    //
    // `UnsafeConstants.PAGE_SIZE` needs no such line: it reads 4096 on both
    // VMs, which is why `pageSize()` retires in the same wave that
    // `addressSize()` could not.
    r.register(u2, "<clinit>", "()V", |ctx, _args| {
        let class_name = "jdk/internal/misc/Unsafe";
        let unsafe_obj = try_alloc_concurrent_synthetic(ctx, class_name, 0)?;
        ctx.set_static_field_by_name(class_name, "theUnsafe", Value::Object(Some(unsafe_obj)));
        ctx.set_static_field_by_name(class_name, "ADDRESS_SIZE", Value::Int(UNSAFE_ADDRESS_SIZE));
        Ok(None)
    });
    r.register(
        u2,
        "getUnsafe",
        "()Ljdk/internal/misc/Unsafe;",
        |ctx, _args| {
            let class_name = "jdk/internal/misc/Unsafe";
            ctx.ensure_class_initialized(class_name)?;
            let class_id = match ctx.class_id_by_name(class_name) {
                Some(id) => id,
                None => {
                    return Ok(Some(Value::Object(Some(try_alloc_concurrent_synthetic(
                        ctx, class_name, 0,
                    )?))))
                }
            };
            let value = match ctx.static_field_index_by_name(class_id, "theUnsafe") {
                Some(idx) => ctx.get_static_field(class_id, idx),
                None => Value::Object(Some(try_alloc_concurrent_synthetic(ctx, class_name, 0)?)),
            };
            Ok(Some(value))
        },
    );
    r.register(
        u2,
        "objectFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
        native_unsafe_object_field_offset,
    );
    r.register(
        u2,
        "staticFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
        native_unsafe_static_field_offset,
    );
    r.register(
        u2,
        "arrayBaseOffset",
        "(Ljava/lang/Class;)I",
        native_unsafe_array_base_offset,
    );
    r.register(
        u2,
        "arrayIndexScale",
        "(Ljava/lang/Class;)I",
        native_unsafe_array_index_scale,
    );
    r.register_with_kind(
        u2,
        "compareAndSetInt",
        "(Ljava/lang/Object;JII)Z",
        native_unsafe_cas_int,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "compareAndSetLong",
        "(Ljava/lang/Object;JJJ)Z",
        native_unsafe_cas_long,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "compareAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_cas_object,
        cratonvm_native_api::NativeKind::Bridge,
    );
    // THE SUB-WORD ATOMICS. These four are not a convenience: without them the
    // byte/short/char/boolean atomic family is BROKEN, and two of its members
    // do not return at all.
    //
    // The JDK implements `compareAndSetByte` in bytecode by masking the 32-bit
    // word that contains the byte:
    //
    //     long wordOffset = offset & ~3;
    //     int  shift      = (int)(offset & 3) << 3;
    //     ... getIntVolatile(o, wordOffset) ... weakCompareAndSetInt(...)
    //
    // That arithmetic is only meaningful when `offset` is a BYTE offset into an
    // object. CratonVM's `objectFieldOffset` returns a SLOT INDEX, so
    // `offset & ~3` names a DIFFERENT FIELD, the masked compare never matches,
    // and every caller built on it either lies or spins. MEASURED against
    // HotSpot 25.0.4+7 with `probes/UnsafeSubwordProbe.java`, one call per
    // process behind a timeout:
    //
    //     compareAndSetByte(right witness)   HotSpot true    CratonVM FALSE
    //     compareAndExchangeByte             HotSpot 10      CratonVM 0
    //     getAndSetByte                      HotSpot 10      CratonVM NEVER RETURNED
    //     getAndAddByte (a registered native) HotSpot 10     CratonVM 10   <- the control
    //
    // The control is what identifies the mechanism: `getAndAddByte` is
    // registered here and is correct, and its neighbours differ only in going
    // through the JDK's word-masking bytecode instead.
    //
    // Registering the CAS layer is sufficient for the whole family, because
    // everything above it delegates rather than re-deriving the offset:
    // `weakCompareAndSetByte*` -> `compareAndSetByte`; `compareAndSetBoolean`
    // -> `compareAndSetByte`; `compareAndSetChar` -> `compareAndSetShort`;
    // `getAndSet*` and `getAndBitwise*` -> `weakCompareAndSet*`. Those
    // delegations are VERIFIED by the probe, not assumed -- it asks char,
    // boolean, weak, getAndSet and getAndBitwise at every width.
    //
    // The bodies are the int ones unchanged: a byte field's slot holds
    // `Value::Int`, and the caller widens `B` to `Int` at the call boundary, so
    // the comparison is already width-correct. What was missing was a
    // registration, not an implementation.
    //
    // SBR3 wave 2, lane unsafecas: for an ARRAY byte lane (a byte or short
    // inside an `int[]`/`long[]`/`short[]`/`char[]` element, a short across two
    // `byte[]` elements) "the int ones unchanged" was wrong, and so was the
    // plain/volatile access beneath them: see `SubwordLane`. The three that
    // answer for the exchange layer (`compareAndExchangeByte`/`Short`,
    // `compareAndSetShort`) are now REVIEWED `Intrinsic`s, not `Bridge`s: the
    // JDK's word-masking bytecode cannot replace them while `objectFieldOffset`
    // answers a slot index, so they stay, and their bodies are HotSpot-exact on
    // fields, statics, raw memory and every array lane (`Sbr3UnsafecasCae`).
    // `compareAndSetByte` stays a `Bridge` retired in favour of the real
    // `compareAndExchangeByte(..) == expected` bytecode over the intrinsic.
    r.register_with_kind(
        u2,
        "compareAndSetByte",
        "(Ljava/lang/Object;JBB)Z",
        native_unsafe_cas_byte,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "compareAndSetShort",
        "(Ljava/lang/Object;JSS)Z",
        native_unsafe_cas_short,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register_with_kind(
        u2,
        "compareAndExchangeByte",
        "(Ljava/lang/Object;JBB)B",
        native_unsafe_cae_byte,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register_with_kind(
        u2,
        "compareAndExchangeShort",
        "(Ljava/lang/Object;JSS)S",
        native_unsafe_cae_short,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register_with_kind(
        u2,
        "getIntVolatile",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_volatile_arena,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putIntVolatile",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile_arena,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "getLongVolatile",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_volatile_arena,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putLongVolatile",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_volatile_arena,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "getReferenceVolatile",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object_volatile,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putReferenceVolatile",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object_volatile,
        cratonvm_native_api::NativeKind::Bridge,
    );
    // Acquire/Release variants — same as volatile in our single-threaded model
    r.register(
        u2,
        "getReferenceAcquire",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object_volatile,
    );
    r.register(
        u2,
        "putReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object_volatile,
    );
    r.register(
        u2,
        "getIntAcquire",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_volatile,
    );
    r.register(
        u2,
        "putIntRelease",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile,
    );
    r.register(
        u2,
        "getLongAcquire",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_volatile,
    );
    r.register(
        u2,
        "putLongRelease",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_volatile,
    );
    // Opaque variants — also same semantics for us
    r.register(
        u2,
        "getReferenceOpaque",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object_volatile,
    );
    r.register(
        u2,
        "putReferenceOpaque",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object_volatile,
    );
    r.register(
        u2,
        "getIntOpaque",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_volatile,
    );
    r.register(
        u2,
        "putIntOpaque",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile,
    );
    r.register_with_kind(
        u2,
        "getReference",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putReference",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "getInt",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putInt",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "getLong",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putLong",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "getShort",
        "(Ljava/lang/Object;J)S",
        native_unsafe_get_short_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putShort",
        "(Ljava/lang/Object;JS)V",
        native_unsafe_put_short_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "getChar",
        "(Ljava/lang/Object;J)C",
        native_unsafe_get_char_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putChar",
        "(Ljava/lang/Object;JC)V",
        native_unsafe_put_char_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    // Unaligned access variants (C16). When the target is a `byte[]` (every
    // HeapByteBuffer's backing store) a typed read/write must assemble or
    // scatter N consecutive elements honoring endianness — the multi-byte
    // `*_mb` handlers do exactly that and fall back to the generic
    // element/field natives for non-byte-array targets. The trailing
    // `bigEndian` boolean is decoded by the `*_mb` handlers; the 2-arg form
    // uses native (little-endian) order.
    r.register(
        u2,
        "getIntUnaligned",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_mb,
    );
    r.register(
        u2,
        "getIntUnaligned",
        "(Ljava/lang/Object;JZ)I",
        native_unsafe_get_int_mb,
    );
    r.register(
        u2,
        "putIntUnaligned",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_mb,
    );
    r.register(
        u2,
        "putIntUnaligned",
        "(Ljava/lang/Object;JIZ)V",
        native_unsafe_put_int_mb,
    );
    r.register(
        u2,
        "getLongUnaligned",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_mb,
    );
    r.register(
        u2,
        "getLongUnaligned",
        "(Ljava/lang/Object;JZ)J",
        native_unsafe_get_long_mb,
    );
    r.register(
        u2,
        "putLongUnaligned",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_mb,
    );
    r.register(
        u2,
        "putLongUnaligned",
        "(Ljava/lang/Object;JJZ)V",
        native_unsafe_put_long_mb,
    );
    r.register(
        u2,
        "getShortUnaligned",
        "(Ljava/lang/Object;J)S",
        native_unsafe_get_short_mb,
    );
    r.register(
        u2,
        "getShortUnaligned",
        "(Ljava/lang/Object;JZ)S",
        native_unsafe_get_short_mb,
    );
    r.register(
        u2,
        "putShortUnaligned",
        "(Ljava/lang/Object;JS)V",
        native_unsafe_put_short_mb,
    );
    r.register(
        u2,
        "putShortUnaligned",
        "(Ljava/lang/Object;JSZ)V",
        native_unsafe_put_short_mb,
    );
    r.register(
        u2,
        "getCharUnaligned",
        "(Ljava/lang/Object;J)C",
        native_unsafe_get_char_mb,
    );
    r.register(
        u2,
        "getCharUnaligned",
        "(Ljava/lang/Object;JZ)C",
        native_unsafe_get_char_mb,
    );
    r.register(
        u2,
        "putCharUnaligned",
        "(Ljava/lang/Object;JC)V",
        native_unsafe_put_char_mb,
    );
    r.register(
        u2,
        "putCharUnaligned",
        "(Ljava/lang/Object;JCZ)V",
        native_unsafe_put_char_mb,
    );
    r.register(u2, "storeFence", "()V", native_unsafe_fence);
    r.register(u2, "loadFence", "()V", native_unsafe_fence);
    r.register_with_kind(
        u2,
        "fullFence",
        "()V",
        native_unsafe_fence,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "park",
        "(ZJ)V",
        native_unsafe_park,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "unpark",
        "(Ljava/lang/Object;)V",
        native_unsafe_unpark,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "allocateInstance",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        |ctx, args| {
            // Interpreter round i2 wave 63, lane L1: JVMTI `VMObjectAlloc`.
            let made = native_unsafe_allocate_instance(ctx, args);
            crate::vm_object_alloc_event(ctx, made)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getAndAddInt",
        "(Ljava/lang/Object;JI)I",
        native_unsafe_get_and_add_int,
    );
    r.register(
        u2,
        "getAndSetInt",
        "(Ljava/lang/Object;JI)I",
        native_unsafe_get_and_set_int,
    );
    r.register(
        u2,
        "getAndAddLong",
        "(Ljava/lang/Object;JJ)J",
        native_unsafe_get_and_add_long,
    );
    r.register(
        u2,
        "getAndSetLong",
        "(Ljava/lang/Object;JJ)J",
        native_unsafe_get_and_set_long,
    );
    r.register(
        u2,
        "getAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
        native_unsafe_get_and_set_object,
    );
    r.register(
        u2,
        "copyMemory",
        "(Ljava/lang/Object;JLjava/lang/Object;JJ)V",
        unsafe_natives::native_unsafe_copy_memory_consolidated,
    );
    r.register(u2, "pageSize", "()I", native_unsafe_page_size);
    r.register(u2, "addressSize", "()I", native_unsafe_address_size);

    // --- Remaining Unsafe methods (Phase 44) ---

    // allocateMemory / reallocateMemory / freeMemory — off-heap memory, all
    // backed by the arena store (the allocate/realloc bodies delegate to the
    // consolidated natives, which `register_consolidated_off_heap_store`
    // registers over these later anyway).
    r.register(u, "allocateMemory", "(J)J", native_unsafe_allocate_memory);
    r.register(
        u,
        "reallocateMemory",
        "(JJ)J",
        native_unsafe_allocate_memory_realloc,
    );
    r.register(u, "freeMemory", "(J)V", native_unsafe_free_memory_ext);
    r.register_with_kind(
        u2,
        "allocateMemory0",
        "(J)J",
        native_unsafe_allocate_memory,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "reallocateMemory0",
        "(JJ)J",
        native_unsafe_allocate_memory_realloc,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "freeMemory0",
        "(J)V",
        native_unsafe_free_memory_ext,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // RETIRED 2026-08-29. Every triple below stood in front of a method NO
    // supported JDK image declares, so nothing could ever dispatch to it.
    // Evidence, in the order the campaign requires it:
    //
    //   * ABSENT from JDK 17.0.20.1+1, 21.0.12+8 and 25.0.4+7 -- measured by
    //     `UnsafeImageCensus` over all 212 registration triples, not read off
    //     one image. Three OTHER rows of that census went the other way
    //     (`weakCompareAndSetObject`, `sun` `ensureClassInitialized` and
    //     `shouldBeInitialized` are bytecode on 17 and 21 and absent only on
    //     25), which is why the census had to be run before any of this.
    //   * the synthetic-JDK mode fabricates these two classes with
    //     `synthetic_stub_ctor_methods`, a fixed per-class list that gives
    //     `Unsafe` only `<clinit>` -- so the fourth "image" does not declare
    //     them either.
    //   * 0 invocations across 118 corpus vectors in BOTH modes, against live
    //     controls in the same runs: `objectFieldOffset1` 964/1044 in all 118,
    //     `compareAndSetInt` ~35k, and -- the tightest control available --
    //     `park(ZJ)V` at 781 invocations in 16 vectors while the
    //     `park(Ljava/lang/Object;J)V` retired here is 0. Two rows differing
    //     only by descriptor, one live and one dead.
    //   * registrar history: all of them trace to the initial open-source
    //     commit. No diagnosed defect is behind any of them.
    //
    // The handler functions are left in place (the crate allows `dead_code`)
    // so a future image that declares one of these can be served by
    // re-registering a line, rather than by rediscovering the body.
    // `monitorEnter` / `monitorExit`: registrations retired here.

    // throwException — throws a checked exception without declaring it
    r.register(
        u,
        "throwException",
        "(Ljava/lang/Throwable;)V",
        native_unsafe_throw_exception,
    );
    r.register_with_kind(
        u2,
        "throwException",
        "(Ljava/lang/Throwable;)V",
        native_unsafe_throw_exception,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // shouldBeInitialized(Class) — "is this class still uninitialized?".
    //
    // Now reads the VM's real per-class init state (see
    // `native_unsafe_should_be_initialized`). The previous constant `false`
    // was only accidentally right: it happened to match the dominant caller
    // (`DirectMethodHandle$EnsureInitialized.computeValue` asks only AFTER
    // calling `ensureClassInitialized`), but every other caller —
    // `InvokerBytecodeGenerator`/`MethodHandles` deciding whether a `<clinit>`
    // barrier is still needed — was told "already initialized" about classes
    // whose `<clinit>` had not run.
    r.register(
        u,
        "shouldBeInitialized",
        "(Ljava/lang/Class;)Z",
        native_unsafe_should_be_initialized,
    );
    r.register_with_kind(
        u2,
        "shouldBeInitialized0",
        "(Ljava/lang/Class;)Z",
        native_unsafe_should_be_initialized,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // staticFieldBase — returns the declaring class's mirror, which is what the
    // matching `staticFieldOffset` above encodes against.
    //
    // These two used to be always-null closures. In essential-natives mode that
    // was invisible (`unsafe_natives::register_unsafe_wp1_2` runs later and
    // re-registers the real impl), but synthetic mode calls THIS function a
    // second time from `register_synthetic_overrides` — i.e. AFTER wp1.2 — so
    // the null closure won and every `staticFieldBase(Field)` in synthetic mode
    // returned null. Point both at the real implementation instead.
    r.register(
        u,
        "staticFieldBase",
        "(Ljava/lang/reflect/Field;)Ljava/lang/Object;",
        unsafe_natives::native_unsafe_static_field_base,
    );
    r.register(
        u2,
        "staticFieldBase",
        "(Ljava/lang/reflect/Field;)Ljava/lang/Object;",
        unsafe_natives::native_unsafe_static_field_base,
    );

    // ensureClassInitialized — trigger class initialization
    r.register(
        u,
        "ensureClassInitialized",
        "(Ljava/lang/Class;)V",
        native_unsafe_ensure_class_initialized,
    );
    r.register(
        u2,
        "ensureClassInitialized",
        "(Ljava/lang/Class;)V",
        native_unsafe_ensure_class_initialized,
    );

    // defineClass — define a class from byte array (delegate to ClassLoader)
    // jdk.internal.misc.CDS. CratonVM has its own archive format (see
    // `native-builtins/src/cds.rs` + `SharedVm::dump_cds_archive`), so two of
    // these three predicates are NOT constants: `-Xshare:on/auto` with a
    // loadable archive sets `jdk.internal.vm.cds.enabled`, and `-Xshare:dump`
    // sets `jdk.internal.vm.cds.dumping` (both in `vm/src/vm/vm_init.rs`).
    // They are read here, matching `cds.rs::native_cds_is_sharing_enabled`.
    //
    // `isDumpingClassList0` stays `false`: it reflects HotSpot's
    // `-XX:DumpLoadedClassList=<file>`, which CratonVM's CLI does not accept
    // at all, so there is no state to read.
    //
    // The archive HOOKS (`logLambdaFormInvoker`, `initializeFromArchive`,
    // `defineArchivedModules`, `dumpClassList`, `dumpDynamicArchive`) stay
    // no-ops even when an archive IS mapped: CratonVM's archive stores class
    // BYTES only — no archived heap objects, no archived module graph, no
    // lambda-form invoker list — and every `java.base` caller is written for
    // exactly that ("restore returned nothing, build it normally").
    //
    // NOTE: overlapping registration sets also exist in `lib.rs`
    // (register_essential_natives_with_shims), `phases_early.rs::
    // register_core_stdlib_extras` and `cds.rs::register_cds_natives`, all of
    // which run AFTER this one. DO NOT delete this block as a pure duplicate:
    // `isSharingEnabled0` and `defineArchivedModules` are registered here and
    // NOWHERE ELSE on the real-JDK path (the `lib.rs` block registers
    // `isSharingEnabled`, without the `0`, and no `defineArchivedModules`;
    // `phases_early`/`cds.rs` only run under the `synthetic-jdk` feature), so
    // those two are live. The other seven are shadowed in both modes.
    let cds_cls = "jdk/internal/misc/CDS";
    r.register(cds_cls, "isDumpingClassList0", "()Z", native_return_false);
    r.register(
        cds_cls,
        "isDumpingArchive0",
        "()Z",
        native_cds_is_dumping_archive0,
    );
    r.register(
        cds_cls,
        "isSharingEnabled0",
        "()Z",
        native_cds_is_sharing_enabled0,
    );
    r.register_with_kind(
        cds_cls,
        "logLambdaFormInvoker",
        "(Ljava/lang/String;)V",
        native_noop,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        cds_cls,
        "initializeFromArchive",
        "(Ljava/lang/Class;)V",
        native_noop,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        cds_cls,
        "defineArchivedModules",
        "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
        native_noop,
        cratonvm_native_api::NativeKind::Bridge,
    );
    // `getRandomSeedForDumping()` — 0 means "not dumping", and 0 is the right
    // answer here even under `-Xshare:dump`. Its one consumer is
    // `java.util.ImmutableCollections`, which uses a NON-zero seed to pin
    // `SALT32L` (Set/Map iteration order) so an archived heap is bit-identical
    // across dump and use, and falls back to `System.nanoTime()` on 0.
    // CratonVM's archive holds class bytes only — no archived
    // `ImmutableCollections` state — so pinning the salt would remove
    // iteration-order randomisation from an ordinary run and buy nothing.
    r.register_with_kind(
        cds_cls,
        "getRandomSeedForDumping",
        "()J",
        |_ctx, _args| Ok(Some(Value::Long(0))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        cds_cls,
        "dumpClassList",
        "(Ljava/lang/String;)V",
        native_noop,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        cds_cls,
        "dumpDynamicArchive",
        "(Ljava/lang/String;)V",
        native_noop,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // defineClass / defineClass0 — real `define_class_full`-routed impl. These
    // were always-null closures whose only saving grace was that
    // `unsafe_natives::register_unsafe_define_class` runs last on both the
    // essential and the synthetic path and overwrote them. Registering the real
    // function here removes that ordering dependency: ByteBuddy's class
    // injection no longer silently gets `null` back if a future registrar lands
    // after this one.
    r.register_with_kind(
        u2,
        "defineClass0",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        unsafe_natives::native_unsafe_define_class,
        cratonvm_native_api::NativeKind::Bridge,
    );

    // defineAnonymousClass — legacy API for defining hidden classes (used by old Lambda/invoke)
    // Signature: defineAnonymousClass(Class<?> hostClass, byte[] data, Object[] cpPatches) -> Class<?>
    fn native_unsafe_define_anonymous_class(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        use std::sync::atomic::Ordering;
        const CAFEBABE: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];

        // args: [unsafe_this, hostClass, byte[], cpPatches]
        let byte_array = match args.get(2) {
            Some(Value::Object(Some(arr))) => *arr,
            _ => return Ok(Some(Value::Object(None))),
        };
        let length = ctx.array_length(byte_array);
        let mut class_bytes = Vec::with_capacity(length);
        for i in 0..length {
            match ctx.get_array_element(byte_array, i) {
                Value::Int(b) => class_bytes.push(b as u8),
                _ => class_bytes.push(0),
            }
        }
        if class_bytes.len() < 4 || class_bytes[0..4] != CAFEBABE {
            return Ok(Some(Value::Object(None)));
        }
        // cpPatches (arg 3) is ignored — legacy constant pool patching not needed.
        let id = crate::classloader::HIDDEN_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
        let anon_name = format!("anonymous/0x{:x}", id);
        match ctx.define_class_from_bytes(&anon_name, &class_bytes) {
            Some(cid) => {
                ctx.set_class_hidden(cid);
                let mirror = ctx.get_class_mirror(cid);
                Ok(Some(Value::Object(Some(mirror))))
            }
            None => Ok(Some(Value::Object(None))),
        }
    }
    // `defineAnonymousClass`: registration retired 2026-08-29, see the note on
    // the `monitorEnter` retirement above for the evidence. This was the second
    // of two registrations of this triple; the other was in `unsafe_natives.rs`.
    r.set_category(__prev_cat);
}

/// Extract the field offset (slot index) from args at the given position.
/// In our VM, offset = slot index (not byte offset).
pub(crate) fn unsafe_offset(args: &[Value], pos: usize) -> usize {
    // Field-slot offsets in this VM are small logical indices (or bounded
    // array byte offsets). When compact-value drift surfaces a `Double/Float`
    // with unrelated bit pattern, `to_bits() as usize` can become a huge
    // index and crash GC heap access with OOB panic. Keep the defensive
    // decode, but clamp clearly-invalid offsets to 0 (HotSpot-style callers
    // already fail CAS/get semantics safely on wrong offsets).
    const MAX_REASONABLE_OFFSET: usize = 1 << 30; // 1 Gi slot/byte cap
                                                  // C32: the Buffer.address probe offset is an intentional high-range
                                                  // sentinel (`BUFFER_ADDRESS_SENTINEL`) minted by
                                                  // `native_unsafe_object_field_offset`. It is far above
                                                  // `MAX_REASONABLE_OFFSET`; the clamp below would otherwise rewrite it
                                                  // to 0, defeating the sentinel check in `native_unsafe_get_long` and
                                                  // making Netty's `PlatformDependent0$3.run()` read 0 → return null →
                                                  // NPE in `PlatformDependent0.<clinit>`. Pass the sentinel through
                                                  // verbatim so the sentinel-aware reader can answer non-zero.
    let clamp = |u: usize| {
        if u == BUFFER_ADDRESS_SENTINEL || u <= MAX_REASONABLE_OFFSET {
            u
        } else if unsafe_arena_contains(u as i64) {
            // DF07: the `base == null` form of `Unsafe.get*/put*(Object,long,..)`
            // (used by `ScopedMemoryAccess` → `DirectByteBuffer.put(byte)`/`get(byte)`
            // for transfers of <=6 elements) passes a FULL off-heap ADDRESS here,
            // not a field-slot index. An Unsafe-arena handle (ARENA_TAG bit 62 set,
            // so always > MAX_REASONABLE_OFFSET) is a legitimate such address —
            // clamping it to 0 made the JDK-25 NioSocketImpl temp DirectByteBuffer
            // (FFM/`newDirectByteBuffer`-wrapped arena handle) put its bytes at
            // address 0 (lost) while `Net.write0` read the arena handle (zeros),
            // so every small real-`Socket` write silently delivered zeros. A live
            // arena handle is never a field offset, so passing it through is safe.
            u
        } else {
            0
        }
    };
    match args.get(pos) {
        Some(Value::Long(off)) => {
            if *off < 0 {
                0
            } else {
                clamp(*off as usize)
            }
        }
        Some(Value::Int(off)) => {
            if *off < 0 {
                0
            } else {
                clamp(*off as usize)
            }
        }
        // T19.H1 / T10.9.E — accept a Double/Float with the bit pattern of
        // a 64-bit long. CompactValue on the invoke-argument boundary is
        // the one remaining decode drift site: invoke arg-pop still uses
        // `stack.pop()` which calls `to_value`, surfacing untagged long
        // bits as `Value::Double`. T10.9.E's descriptor-aware field
        // getter in `NativeContextImpl::get_field` eliminates drift when
        // natives read a long via `ctx.get_field` — but the caller-side
        // argument decode lives in `execute_invoke` / `execute_invokestatic`
        // which are outside T10.9.E's ownership, so we keep the
        // bit-reinterpreting arms as a defensive bridge.
        //
        // Removal prerequisites: invoke arg-pop must parse the method
        // descriptor and pop each arg via a descriptor-aware helper
        // instead of the generic `.pop()?`. Once that lands, every
        // `offset: long` parameter arrives as `Value::Long` and these
        // arms become unreachable.
        Some(Value::Double(d)) => {
            if d.is_finite() {
                let n = *d as i64;
                if n >= 0 && (n as f64) == *d {
                    return clamp(n as usize);
                }
            }
            clamp(d.to_bits() as usize)
        }
        Some(Value::Float(f)) => {
            if f.is_finite() {
                let n = *f as i64;
                if n >= 0 && (n as f32) == *f {
                    return clamp(n as usize);
                }
            }
            clamp(f.to_bits() as usize)
        }
        _ => 0,
    }
}

/// Convert a byte offset (as used by `Unsafe` on arrays) to an array element
/// index. Real-JDK classes like `ConcurrentHashMap` compute offsets via
/// `(i << ASHIFT) + ABASE` where `ABASE=16` (arrayBaseOffset) and `ASHIFT`
/// is `log2(arrayIndexScale)` for the element type. Our VM's
/// `get_array_element` / `set_array_element` take element indices, not byte
/// offsets, so we need to undo that transform.
///
/// **K1-family fix (CHM resize):** the prior heuristic
/// `if offset < len { return offset; }` mis-interpreted byte offsets that
/// happened to be smaller than the array length. For a 32-bucket
/// `ConcurrentHashMap` table, byte offsets 16, 24 (for indices 0, 1) are
/// less than length 32, so they were returned as-is — addressing slots
/// 16, 24 instead of 0, 1.  Symptom: after CHM's first transfer pass the
/// keys whose new-table indices have byte offsets in `[ABASE, len)` are
/// orphaned (`get` returns null).
///
/// New rule: prefer the byte-offset decode (`(offset - ABASE) / scale`)
/// whenever the offset is a *plausible* byte offset — i.e. `offset >= ABASE`,
/// aligned to `scale`, and the decoded index is in range `0..len`.  Only
/// fall back to the legacy "offset is already an index" path when the
/// offset is below `ABASE` (which is impossible for a real `Unsafe`-style
/// byte offset, but pre-existing synthetic stub call sites in our codebase
/// pass raw indices like 0, 1, 2 directly).
pub(crate) fn unsafe_array_index_from_offset(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
) -> usize {
    let len = ctx.array_length(obj);
    let scale = match ctx.heap_element_type_of(obj) {
        cratonvm_types::ArrayElementType::Boolean | cratonvm_types::ArrayElementType::Byte => {
            1usize
        }
        cratonvm_types::ArrayElementType::Char | cratonvm_types::ArrayElementType::Short => 2usize,
        cratonvm_types::ArrayElementType::Int | cratonvm_types::ArrayElementType::Float => 4usize,
        cratonvm_types::ArrayElementType::Long
        | cratonvm_types::ArrayElementType::Double
        | cratonvm_types::ArrayElementType::Reference => 8usize,
    };
    // ABASE is 16 in `native_unsafe_array_base_offset`; matching JDK layout.
    const ABASE: usize = 16;
    if offset >= ABASE {
        let raw_idx = offset - ABASE;
        if raw_idx % scale == 0 {
            let decoded = raw_idx / scale;
            if decoded < len {
                return decoded;
            }
        }
        // Byte-offset shape but out of bounds — return the decoded index so
        // the caller's bounds check fails cleanly rather than aliasing a
        // valid slot.
        (offset - ABASE) / scale
    } else {
        // offset < ABASE — cannot be a byte offset (real `Unsafe` byte
        // offsets always start at ABASE). Treat as a raw element index for
        // the legacy synthetic-stub callers that pass 0/1/2 directly.
        offset
    }
}

/// Decode a JVM-controlled `Unsafe` byte offset into an array element index
/// AND validate it against the array's length, all within this crate.
///
/// `unsafe_array_index_from_offset` deliberately returns the decoded
/// (possibly out-of-range) index when the offset is out of bounds so a
/// downstream bounds check can fail cleanly. But several `Unsafe`
/// array get/set/getAndSet/getAndAdd/CAS call sites fed that index
/// straight into `ctx.get_array_element` / `ctx.set_array_element` /
/// `ctx.compare_and_swap_field` with no explicit check — making memory
/// safety depend entirely on the host accessor validating the index (an
/// out-of-crate invariant). If the host accessor trusted the index this
/// would be an OOB read/write primitive reachable from Java via a forged
/// byte offset.
///
/// This helper closes that gap: it decodes the offset and returns
/// `Some(idx)` only when `idx < array_length`. On any out-of-bounds
/// offset it returns `None`, and the caller short-circuits to a benign
/// JDK-appropriate result (mirroring how the non-array / type-mismatch
/// arms in these same handlers already fall back to a null/zero/no-op
/// sentinel) instead of touching the heap. Out-of-bounds `Unsafe` access
/// is undefined behaviour in the JDK, so a benign sentinel is a sound
/// choice and keeps the hot path branch-light.
#[inline]
pub(crate) fn unsafe_checked_array_index(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
) -> Option<usize> {
    let idx = unsafe_array_index_from_offset(ctx, obj, offset);
    if idx < ctx.array_length(obj) {
        Some(idx)
    } else {
        None
    }
}

/// Extract the object from args at the given position.
pub(crate) fn unsafe_obj(args: &[Value], pos: usize) -> Option<cratonvm_types::ObjectRef> {
    match args.get(pos) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    }
}

// ── Byte field offsets (JIT round 20 wave 8c, lane huntDisp) ──────────────
//
// Page `jr20-unsafe-sub-field-offsets-alias-neighbouring-slots-FIXED-20261012.md`.
// HotSpot's `objectFieldOffset` is a BYTE offset, so `offset + k` is byte `k`
// of that field. CratonVM's answer was the slot index, so `offset + 1` named
// the next field. Under `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` (one per-VM
// answer, `NativeSystemAccess::unsafe_byte_field_offsets`) every offset this
// VM hands out for an INSTANCE field is `16 + slot * 8` and every native that
// takes one decodes `(slot, k)` with [`unsafe_field_slot`]: every slot is at
// most 8 bytes wide, so `offset + k` (`k < 8`) stays inside its field and two
// fields never share an offset; `16` keeps offset 0 from naming slot 0. The
// slot stays the key of every heap accessor. Arrays (real byte offsets),
// statics and minted offsets (the synthetic band) and the `Buffer.address`
// sentinel are not field offsets and pass through unchanged. With the switch
// off both directions are the identity: today's slot indices exactly.

/// The byte offset of the first instance-field slot under byte offsets.
pub(crate) const UNSAFE_BYTE_FIELD_OFFSET_BASE: usize = 16;

/// The `Unsafe` offset this VM answers for instance-field `slot`.
pub(crate) fn unsafe_encode_field_offset(byte_offsets: bool, slot: usize) -> usize {
    if byte_offsets {
        UNSAFE_BYTE_FIELD_OFFSET_BASE + slot * 8
    } else {
        slot
    }
}

/// `(slot, byte within the slot)` of an instance-field `offset`. An offset
/// below the base decodes to a huge slot every heap accessor bounds-checks
/// away (no field), never to slot 0.
pub(crate) fn unsafe_decode_field_offset(byte_offsets: bool, offset: usize) -> (usize, usize) {
    if !byte_offsets {
        return (offset, 0);
    }
    let rel = offset.wrapping_sub(UNSAFE_BYTE_FIELD_OFFSET_BASE);
    (rel >> 3, rel & 7)
}

/// [`unsafe_decode_field_offset`] for the field arm of a native whose base is
/// `obj`: the identity for an array, a synthetic (minted / static) offset, the
/// `Buffer.address` sentinel, and whenever this VM's switch is off.
pub(crate) fn unsafe_field_slot(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
) -> (usize, usize) {
    if !ctx.unsafe_byte_field_offsets()
        || offset == BUFFER_ADDRESS_SENTINEL
        || is_synthetic_offset(offset)
        || ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
    {
        return (offset, 0);
    }
    unsafe_decode_field_offset(true, offset)
}

/// Byte width of a primitive field descriptor; `None` for a reference.
fn unsafe_primitive_width(desc: u8) -> Option<usize> {
    match desc {
        b'B' | b'Z' => Some(1),
        b'S' | b'C' => Some(2),
        b'I' | b'F' => Some(4),
        b'J' | b'D' => Some(8),
        _ => None,
    }
}

/// The little-endian bits of a primitive field value (an `int`-family slot
/// zero-extended from 32 bits; the field's own width masks it).
fn unsafe_value_bits(v: Value) -> Option<u64> {
    // Casts: bit reinterpretation of each primitive kind.
    match v {
        Value::Int(i) => Some(i as u32 as u64),
        Value::Long(l) => Some(l as u64),
        Value::Float(f) => Some(f.to_bits() as u64),
        Value::Double(d) => Some(d.to_bits()),
        _ => None,
    }
}

/// The `Value` a field of descriptor `desc` holds for its low bytes `bits`.
fn unsafe_value_of_bits(desc: u8, bits: u64) -> Option<Value> {
    // Casts: each field's value is its low bytes, re-extended as its type.
    Some(match desc {
        b'B' => Value::Int(bits as u8 as i8 as i32),
        b'Z' => Value::Int(bits as u8 as i32),
        b'S' => Value::Int(bits as u16 as i16 as i32),
        b'C' => Value::Int(bits as u16 as i32),
        b'I' => Value::Int(bits as u32 as i32),
        b'F' => Value::Float(f32::from_bits(bits as u32)),
        b'J' => Value::Long(bits as i64),
        b'D' => Value::Double(f64::from_bits(bits)),
        _ => return None,
    })
}

/// `width` bytes starting at byte `k` of the PRIMITIVE instance field at
/// `slot` of `obj` (clamped to the field: the bytes HotSpot would read past it
/// belong to the layout around the field, which this VM does not have). The
/// atomic forms are a CAS loop on the whole slot, so they are atomic against
/// every other access of the field, as a CAS on the containing word is.
struct FieldLane {
    obj: ObjectRef,
    slot: usize,
    shift: u32,
    mask: u64,
    desc: u8,
    /// The field's own width in bytes.
    field_width: usize,
}

impl FieldLane {
    fn locate(
        ctx: &dyn NativeContext,
        obj: ObjectRef,
        slot: usize,
        k: usize,
        width: usize,
    ) -> Option<Self> {
        let desc = match ctx.instance_field_descriptor_byte(obj, slot) {
            Some(d) => d,
            None => match ctx.get_field(obj, slot) {
                Value::Int(_) => b'I',
                Value::Long(_) => b'J',
                Value::Float(_) => b'F',
                Value::Double(_) => b'D',
                _ => return None,
            },
        };
        let field_width = unsafe_primitive_width(desc)?;
        if width == 0 || k >= field_width {
            return None;
        }
        let w = width.min(field_width - k);
        Some(Self {
            obj,
            slot,
            // Cast: `k < 8`.
            shift: (k * 8) as u32,
            mask: if w >= 8 {
                u64::MAX
            } else {
                (1u64 << (w * 8)) - 1
            },
            desc,
            field_width,
        })
    }

    /// The lane, zero-extended.
    fn load(&self, ctx: &dyn NativeContext) -> Option<u64> {
        let bits = unsafe_value_bits(ctx.get_field_volatile(self.obj, self.slot))?;
        Some((bits >> self.shift) & self.mask)
    }

    /// Replace the lane with `f(current)` (`None` = leave it), atomically
    /// against the field's other accesses. Returns the lane witnessed and
    /// whether `f` wrote. A slot whose CAS keeps failing on an unchanged value
    /// (a NaN payload the CAS compares by value) ends with the plain store
    /// after 65 536 attempts, as `CRATONVM_VH_RMW_STUCK_GUARD` does.
    fn rmw(
        &self,
        ctx: &mut dyn NativeContext,
        f: &dyn Fn(u64) -> Option<u64>,
    ) -> Option<(u64, bool)> {
        let mut attempts = 0u32;
        loop {
            let old = ctx.get_field_volatile(self.obj, self.slot);
            let bits = unsafe_value_bits(old)?;
            let cur = (bits >> self.shift) & self.mask;
            let Some(upd) = f(cur) else {
                return Some((cur, false));
            };
            let new_bits = (bits & !(self.mask << self.shift)) | ((upd & self.mask) << self.shift);
            let new = unsafe_value_of_bits(self.desc, new_bits)?;
            if ctx.compare_and_swap_field(self.obj, self.slot, old, new) {
                return Some((cur, true));
            }
            attempts += 1;
            if attempts >= 1 << 16 {
                ctx.set_field(self.obj, self.slot, new);
                return Some((cur, true));
            }
            if attempts % 1024 == 0 {
                std::thread::yield_now();
            }
        }
    }
}

/// A read of `width` bytes at byte `k` (`k != 0`) of instance-field `slot`:
/// the bytes, zero-extended; `0` for a reference field or past the field.
fn unsafe_sub_field_load(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    slot: usize,
    k: usize,
    width: usize,
) -> u64 {
    FieldLane::locate(ctx, obj, slot, k, width)
        .and_then(|lane| lane.load(ctx))
        .unwrap_or(0)
}

/// A store of the low `width` bytes of `bits` at byte `k` (`k != 0`) of
/// instance-field `slot`; nothing for a reference field or past the field.
fn unsafe_sub_field_store(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    slot: usize,
    k: usize,
    width: usize,
    bits: u64,
) {
    if let Some(lane) = FieldLane::locate(&*ctx, obj, slot, k, width) {
        let _ = lane.rmw(ctx, &|_| Some(bits));
    }
}

/// The byte at byte offset `offset` of an INSTANCE (non-array) base under
/// byte field offsets; `None` when `offset` names no primitive field byte.
pub(crate) fn unsafe_field_byte_at(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
) -> Option<u8> {
    if offset < UNSAFE_BYTE_FIELD_OFFSET_BASE {
        return None;
    }
    let (slot, k) = unsafe_decode_field_offset(true, offset);
    if slot >= ctx.object_num_fields(obj) {
        return None;
    }
    // Cast: one byte of the lane.
    FieldLane::locate(ctx, obj, slot, k, 1)?
        .load(ctx)
        .map(|b| b as u8)
}

/// Store `byte` at byte offset `offset` of an INSTANCE (non-array) base under
/// byte field offsets; `false`, nothing written, when `offset` names no
/// primitive field byte.
pub(crate) fn unsafe_field_set_byte_at(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
    byte: u8,
) -> bool {
    if offset < UNSAFE_BYTE_FIELD_OFFSET_BASE {
        return false;
    }
    let (slot, k) = unsafe_decode_field_offset(true, offset);
    if slot >= ctx.object_num_fields(obj) {
        return false;
    }
    match FieldLane::locate(&*ctx, obj, slot, k, 1) {
        Some(lane) => lane.rmw(ctx, &|_| Some(u64::from(byte))).is_some(),
        None => false,
    }
}

/// `Unsafe.setMemory` over an INSTANCE (non-array) base under byte field
/// offsets: `value` into each of the `bytes` bytes from `offset` on, field by
/// field (a reference field's bytes are left alone). `false`, nothing
/// written, when the range is not inside the object's fields.
pub(crate) fn unsafe_field_fill_bytes(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
    bytes: usize,
    value: u8,
) -> bool {
    let Some(last) = offset.checked_add(bytes).and_then(|end| end.checked_sub(1)) else {
        return false;
    };
    let num_fields = ctx.object_num_fields(obj);
    let (first_slot, _) = unsafe_decode_field_offset(true, offset);
    let (last_slot, _) = unsafe_decode_field_offset(true, last);
    if bytes == 0
        || offset < UNSAFE_BYTE_FIELD_OFFSET_BASE
        || first_slot >= num_fields
        || last_slot >= num_fields
    {
        return false;
    }
    for at in offset..=last {
        let (slot, k) = unsafe_decode_field_offset(true, at);
        if let Some(lane) = FieldLane::locate(&*ctx, obj, slot, k, 1) {
            let _ = lane.rmw(ctx, &|_| Some(u64::from(value)));
        }
    }
    true
}

// ── Byte offsets of STATIC fields (JIT round 20 wave 11, lane huntDisp) ────
//
// A static field's `Unsafe` offset is a MINTED number in the synthetic band
// (`staticFieldOffset` -> `synthetic_offset_for`, one per field, consecutive),
// registered per VM as `offset -> (class, static index)`; `offset + k` named
// another field's offset or nothing. Under `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`
// the VM answers the minted offset `m` scaled into the upper half of the band,
// `UNSAFE_BYTE_STATIC_OFFSET_BASE + (m - SYNTHETIC_OFFSET_BASE) * 8`, and
// registers that number too, so an exact access resolves through the existing
// registration and `offset + k` (`k < 8`) decodes to byte `k` of the same
// static ([`unsafe_static_sub_target`]). Served on the `staticFieldBase` mirror
// base (the JDK's convention: `synthetic_get` / `_put` / `_cas` and the narrow
// store); a null base keeps whole-field statics only.

/// The first scaled static offset (8-aligned, inside the synthetic band).
pub(crate) const UNSAFE_BYTE_STATIC_OFFSET_BASE: usize = 0x3000_0000;

/// The offset this VM answers for the minted static offset `minted`: scaled
/// (and registered for `class_id`'s static `field_index`) under byte field
/// offsets, `minted` itself otherwise.
pub(crate) fn unsafe_static_offset_answer(
    ctx: &dyn NativeContext,
    minted: usize,
    class_id: ClassId,
    field_index: usize,
) -> usize {
    if !ctx.unsafe_byte_field_offsets() || minted < SYNTHETIC_OFFSET_BASE {
        return minted;
    }
    let scaled = UNSAFE_BYTE_STATIC_OFFSET_BASE + (minted - SYNTHETIC_OFFSET_BASE) * 8;
    if scaled > SYNTHETIC_OFFSET_END - 7 {
        return minted;
    }
    remember_unsafe_static_field_offset(ctx.vm_identity(), scaled, class_id, field_index);
    scaled
}

/// `(class, static index, k)` when `offset` is byte `k != 0` of a static this
/// VM answered a scaled offset for; `None` otherwise (the switch off, any other
/// offset, an exact static offset).
///
/// gcj j1/jt (page `gci-i48-unsafe-byte-lane-static-sub-target-ignores-the-base`):
/// the containing static is resolved against the access's own `base` (the
/// `staticFieldBase` mirror) exactly as the word lane resolves it
/// ([`unsafe_static_field_target_via_base`], gci cl48), so a sub-word access
/// through one copy's mirror never reaches another copy registered under the
/// same offset. `None` (or a base that names no registered class) keeps the
/// latest registration, as before.
pub(crate) fn unsafe_static_sub_target(
    ctx: &dyn NativeContext,
    base: Option<ObjectRef>,
    offset: usize,
) -> Option<(ClassId, usize, usize)> {
    if !ctx.unsafe_byte_field_offsets()
        || !(UNSAFE_BYTE_STATIC_OFFSET_BASE..=SYNTHETIC_OFFSET_END).contains(&offset)
        || offset & 7 == 0
    {
        return None;
    }
    let (class_id, field_index) = unsafe_static_field_target_via_base(ctx, base, offset & !7)?;
    Some((class_id, field_index, offset & 7))
}

/// The declared descriptor byte of `class_id`'s static `field_index` (else
/// the stored value's kind).
fn unsafe_static_desc(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    field_index: usize,
) -> Option<u8> {
    for f in ctx.declared_fields(class_id) {
        if f.is_static && ctx.static_field_index_by_name(class_id, &f.name) == Some(field_index) {
            return f.descriptor.as_bytes().first().copied();
        }
    }
    match ctx.get_static_field(class_id, field_index) {
        Value::Int(_) => Some(b'I'),
        Value::Long(_) => Some(b'J'),
        Value::Float(_) => Some(b'F'),
        Value::Double(_) => Some(b'D'),
        _ => None,
    }
}

/// `(desc, shift, mask)` of `width` bytes at byte `k` of a primitive static,
/// clamped to the field; `None` for a reference or past the field.
fn unsafe_static_lane(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    field_index: usize,
    k: usize,
    width: usize,
) -> Option<(u8, u32, u64)> {
    let desc = unsafe_static_desc(ctx, class_id, field_index)?;
    let field_width = unsafe_primitive_width(desc)?;
    if width == 0 || k >= field_width {
        return None;
    }
    let w = width.min(field_width - k);
    // Cast: `k < 8`.
    Some((
        desc,
        (k * 8) as u32,
        if w >= 8 {
            u64::MAX
        } else {
            (1u64 << (w * 8)) - 1
        },
    ))
}

/// Replace `width` bytes at byte `k` of a static with `f(current)`: a read and
/// a write, as every static access of the `Unsafe` natives is
/// (`unsafe_static_cas`). Returns the lane witnessed and whether `f` wrote.
fn unsafe_static_lane_rmw(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    field_index: usize,
    k: usize,
    width: usize,
    f: &dyn Fn(u64) -> Option<u64>,
) -> Option<(u64, bool)> {
    let (desc, shift, mask) = unsafe_static_lane(&*ctx, class_id, field_index, k, width)?;
    let bits = unsafe_value_bits(ctx.get_static_field(class_id, field_index))?;
    let cur = (bits >> shift) & mask;
    let Some(upd) = f(cur) else {
        return Some((cur, false));
    };
    let new_bits = (bits & !(mask << shift)) | ((upd & mask) << shift);
    ctx.set_static_field(class_id, field_index, unsafe_value_of_bits(desc, new_bits)?);
    Some((cur, true))
}

/// The byte width a value written through `Unsafe` carries.
fn unsafe_value_width(v: Value) -> Option<usize> {
    match v {
        Value::Int(_) | Value::Float(_) => Some(4),
        Value::Long(_) | Value::Double(_) => Some(8),
        _ => None,
    }
}

/// `synthetic_get`'s answer for byte `k != 0` of a static (the bytes from `k`
/// on, low 32 bits: the natives' returns narrow it to the access), or `None`.
pub(crate) fn unsafe_static_sub_get(
    ctx: &dyn NativeContext,
    base: Option<ObjectRef>,
    offset: usize,
) -> Option<Value> {
    let (class_id, field_index, k) = unsafe_static_sub_target(ctx, base, offset)?;
    let bits = unsafe_static_lane(ctx, class_id, field_index, k, 8).and_then(|(_, shift, mask)| {
        unsafe_value_bits(ctx.get_static_field(class_id, field_index)).map(|b| (b >> shift) & mask)
    });
    // Cast: the low 32 bits of the zero-extended bytes.
    Some(Value::Int(bits.unwrap_or(0) as u32 as i32))
}

/// `synthetic_put`'s store of `val` at byte `k != 0` of a static (its own
/// width, clamped to the field). `true` when the offset was such a byte
/// (written, or nothing to write for a reference value).
pub(crate) fn unsafe_static_sub_put(
    ctx: &mut dyn NativeContext,
    base: Option<ObjectRef>,
    offset: usize,
    val: Value,
) -> bool {
    let Some((class_id, field_index, k)) = unsafe_static_sub_target(&*ctx, base, offset) else {
        return false;
    };
    if let (Some(width), Some(bits)) = (unsafe_value_width(val), unsafe_value_bits(val)) {
        let _ = unsafe_static_lane_rmw(ctx, class_id, field_index, k, width, &|_| Some(bits));
    }
    true
}

/// `synthetic_cas`'s CAS at byte `k != 0` of a static, by the width of
/// `expected`; `None` when the offset is not such a byte.
pub(crate) fn unsafe_static_sub_cas(
    ctx: &mut dyn NativeContext,
    base: Option<ObjectRef>,
    offset: usize,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    let (class_id, field_index, k) = unsafe_static_sub_target(&*ctx, base, offset)?;
    let width = unsafe_value_width(expected)?;
    let (ex, nv) = (unsafe_value_bits(expected)?, unsafe_value_bits(new_val)?);
    // The lane's own mask (clamped to the field): the witness is compared
    // with the expected value's bytes that fall inside the field.
    let Some((_, _, mask)) = unsafe_static_lane(&*ctx, class_id, field_index, k, width) else {
        return Some(false);
    };
    let ex = ex & mask;
    Some(
        unsafe_static_lane_rmw(ctx, class_id, field_index, k, width, &|cur| {
            (cur == ex).then_some(nv)
        })
        .is_some_and(|(_, wrote)| wrote),
    )
}

/// The byte / short / int / long `Unsafe` atomics (`Cas`, `Cae`, `Add`, `Set`)
/// on an INSTANCE field under byte field offsets, when the access is not the
/// whole field: an offset inside it (`k != 0`), or an access narrower than it
/// (`compareAndSetByte` at an `int` field's offset). A [`FieldLane`] CAS loop
/// on the containing slot, as HotSpot's CAS on the containing word. `None`
/// (the caller keeps its path) with the switch off, for an array, a null base
/// or a minted offset, and for a whole-field access. `args` as in
/// [`unsafe_arena_atomic`]; `width` is the access's.
pub(crate) fn unsafe_field_lane_atomic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: ArenaAtomic,
    width: usize,
) -> Option<Value> {
    let obj = unsafe_obj(args, 1)?;
    if !ctx.unsafe_byte_field_offsets()
        || ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
    {
        return None;
    }
    let offset = unsafe_offset(args, 2);
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) || offset == BUFFER_ADDRESS_SENTINEL {
        return None;
    }
    let (slot, k) = unsafe_field_slot(&*ctx, obj, offset);
    let witness_of = |v: u64| -> Value {
        // Casts: the witnessed bytes as the access's Java type.
        match width {
            1 => Value::Int(v as u8 as i8 as i32),
            2 => Value::Int(v as u16 as i16 as i32),
            8 => Value::Long(v as i64),
            _ => Value::Int(v as u32 as i32),
        }
    };
    let Some(lane) = FieldLane::locate(&*ctx, obj, slot, k, width) else {
        // A reference field, or past the field: nothing changes.
        return (k != 0).then(|| {
            if op == ArenaAtomic::Cas {
                Value::Int(0)
            } else {
                witness_of(0)
            }
        });
    };
    if k == 0 && width >= lane.field_width {
        return None;
    }
    let mask = lane.mask;
    let arg = |i: usize| -> u64 {
        // Casts: the operand's bits, masked to the lane.
        match args.get(i) {
            Some(Value::Int(v)) => *v as i64 as u64 & mask,
            Some(Value::Long(v)) => *v as u64 & mask,
            Some(Value::Float(f)) => u64::from(f.to_bits()) & mask,
            Some(Value::Double(d)) => d.to_bits() & mask,
            _ => 0,
        }
    };
    let (a, b) = (arg(3), arg(4));
    let (cur, ok) = match op {
        ArenaAtomic::Cas | ArenaAtomic::Cae => lane.rmw(ctx, &|cur| (cur == a).then_some(b))?,
        ArenaAtomic::Add => lane.rmw(ctx, &|cur| Some(cur.wrapping_add(a) & mask))?,
        ArenaAtomic::Set => lane.rmw(ctx, &|_| Some(a))?,
    };
    Some(if op == ArenaAtomic::Cas {
        Value::Int(i32::from(ok))
    } else {
        witness_of(cur)
    })
}

#[inline]
pub(crate) fn unsafe_shard_of<K: std::hash::Hash + ?Sized>(key: &K) -> usize {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    key.hash(&mut h);
    (h.finish() as usize) & UNSAFE_SHARD_MASK
}

#[inline]
pub(crate) fn new_unsafe_sharded_map<K, V>() -> UnsafeShardedMap<K, V> {
    // [(); N].map(...) preserves the const length and produces a fully
    // initialised array without requiring Default on the value type.
    [(); UNSAFE_SHARDS].map(|_| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Key of the per-VM Unsafe offset tables: `(vm_identity, offset)`.
///
/// gc-common w8-a (`common-w7a-unsafe-static-field-targets-and-primitive-side-stores-are-process-global`):
/// offsets are minted per `(class name, field name)` for the whole process
/// (`synthetic_offset_for`), so two VMs asking for the same field get the same
/// number. Every table that maps an offset to something only one VM can
/// interpret -- a `ClassId`, a null-base `int` / `long` slot, the
/// `Thread$ThreadIdentifiers` counter -- therefore leads its key with the VM.
/// The shard is still chosen by the offset alone
/// ([`lock_unsafe_vm_offset_shard`]), so every VM's row for one offset lives
/// under one lock and the hot path pays one lock, as before.
pub(crate) type UnsafeVmOffsetKey = (usize, usize);

/// Lock the shard of a `(vm_identity, offset)`-keyed sharded map that holds
/// `offset` (every VM's row for it). Index the guard with
/// `(ctx.vm_identity(), offset)`.
#[inline]
pub(crate) fn lock_unsafe_vm_offset_shard<V>(
    map: &'static UnsafeShardedMap<UnsafeVmOffsetKey, V>,
    offset: usize,
) -> parking_lot::MutexGuard<'static, rustc_hash::FxHashMap<UnsafeVmOffsetKey, V>> {
    map[unsafe_shard_of(&offset)].lock()
}

/// Offset -> `(class id, static field index)`, per VM. See
/// [`UnsafeVmOffsetKey`]: the `ClassId` is only meaningful in the VM that
/// registered it (class ids are minted per VM from zero), so the map used to
/// hand VM A's null-base static access the slot of whatever class VM B had
/// registered under that id.
///
/// gci cl48 (`gci-i47w-jw1-compatible-child-first-loader-statics-read-back-wrong`):
/// a row holds EVERY class of the VM registered under the offset, latest last.
/// The offset is minted per class NAME (`synthetic_offset_for`), so the eight
/// child-first copies of one class share it. The row used to keep only the
/// last registration: an access based on any other copy's mirror missed it
/// (and fell to the per-mirror side store, which no `getstatic` reads), and a
/// handle that resolves the offset alone reached the last copy's static. A
/// base names its class among the rows ([`unsafe_static_field_target_for_base`]);
/// a base-less access still takes the latest.
fn unsafe_static_field_targets(
) -> &'static UnsafeShardedMap<UnsafeVmOffsetKey, UnsafeStaticFieldRows> {
    static T: std::sync::OnceLock<UnsafeShardedMap<UnsafeVmOffsetKey, UnsafeStaticFieldRows>> =
        std::sync::OnceLock::new();
    T.get_or_init(new_unsafe_sharded_map)
}

/// The classes registered under one `(vm, offset)`, latest last, one entry per
/// class id. See [`unsafe_static_field_targets`].
type UnsafeStaticFieldRows = Vec<UnsafeStaticFieldTarget>;

/// Register `offset` as `class_id`'s static field `field_index` in VM `vm`.
/// A class registered again moves to the end (the latest); another class of
/// the same name joins the row instead of replacing it (gci cl48).
pub(crate) fn remember_unsafe_static_field_offset(
    vm: usize,
    offset: usize,
    class_id: ClassId,
    field_index: usize,
) {
    let mut map = lock_unsafe_vm_offset_shard(unsafe_static_field_targets(), offset);
    let rows = map.entry((vm, offset)).or_default();
    let class_id = class_id.as_u32();
    if let Some(at) = rows.iter().position(|t| t.class_id == class_id) {
        rows.remove(at);
    }
    rows.push(UnsafeStaticFieldTarget {
        class_id,
        field_index,
    });
}

/// The latest registration of `(vm, offset)`, without cloning the row.
fn latest_static_field_row(key: UnsafeVmOffsetKey) -> Option<(UnsafeStaticFieldTarget, bool)> {
    let map = lock_unsafe_vm_offset_shard(unsafe_static_field_targets(), key.1);
    let rows = map.get(&key)?;
    Some((*rows.last()?, rows.len() > 1))
}

/// The registration of `(vm, offset)` for `class_id`, if that class has one.
fn static_field_row_of_class(key: UnsafeVmOffsetKey, class_id: ClassId) -> Option<usize> {
    let map = lock_unsafe_vm_offset_shard(unsafe_static_field_targets(), key.1);
    map.get(&key)?
        .iter()
        .rev()
        .find(|t| t.class_id == class_id.as_u32())
        .map(|t| t.field_index)
}

/// A static-field registration made WITHOUT a VM identity: the VM-less
/// `lib.rs::register_static_field_offset`, kept only until its one caller
/// (`vm/src/vm/vm_util.rs`'s `sun.misc.Unsafe` latch fix-up) passes its VM --
/// `docs/internal/gc-common-round-20260923/applied/handoff-w8a-register-static-field-offset-per-vm.md`.
///
/// It records the class NAME beside the registering VM's id, and a reader in
/// any VM accepts the row only for a class of that name in ITS OWN VM (the
/// base mirror's class, or the recorded id when it names the same class
/// there). A static's index is a property of the class file, so it is the
/// same in every VM that loaded the same class; the id is not.
struct UnscopedStaticFieldTarget {
    class_name: std::sync::Arc<str>,
    class_id: u32,
    field_index: usize,
}

/// Whether any [`UnscopedStaticFieldTarget`] was ever recorded. Lets every
/// per-VM miss (the synthetic-offset hot path in `synthetic_get` / `_put` /
/// `_cas` is one) skip the second lock with one relaxed load.
static UNSCOPED_STATIC_TARGETS_USED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn unscoped_static_field_targets(// addr-keyed: not an address -- synthetic Unsafe static-field offsets.
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<usize, UnscopedStaticFieldTarget>> {
    static T: std::sync::OnceLock<
        // addr-keyed: the accessor's table (see above).
        parking_lot::Mutex<rustc_hash::FxHashMap<usize, UnscopedStaticFieldTarget>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Record a VM-less static-field registration. See
/// [`UnscopedStaticFieldTarget`]; only `lib.rs::register_static_field_offset`
/// calls this.
pub(crate) fn remember_unscoped_static_field_offset(
    offset: usize,
    class_name: &str,
    class_id: ClassId,
    field_index: usize,
) {
    unscoped_static_field_targets().lock().insert(
        offset,
        UnscopedStaticFieldTarget {
            class_name: cratonvm_types::intern_arc(class_name),
            class_id: class_id.as_u32(),
            field_index,
        },
    );
    UNSCOPED_STATIC_TARGETS_USED.store(true, std::sync::atomic::Ordering::Release);
}

/// Resolve a VM-less registration in the calling VM. `class_hint` is the
/// base mirror's class when the access has one; otherwise the recorded id is
/// used only if it names the recorded class in this VM.
fn unscoped_static_field_target(
    ctx: &dyn NativeContext,
    offset: usize,
    class_hint: Option<ClassId>,
) -> Option<(ClassId, usize)> {
    if !UNSCOPED_STATIC_TARGETS_USED.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    let (class_name, recorded, field_index) = {
        let map = unscoped_static_field_targets().lock();
        let t = map.get(&offset)?;
        (t.class_name.clone(), t.class_id, t.field_index)
    };
    let class_id = class_hint.unwrap_or_else(|| ClassId::new(recorded));
    (ctx.class_name_of_id(class_id).as_deref() == Some(&*class_name))
        .then_some((class_id, field_index))
}

/// Per-VM teardown for the offset-keyed Unsafe tables in this file: the
/// offset -> static-field map. Called from `lib.rs::forget_vm_unsafe_side_stores`,
/// which also drops the null-base primitive stores. Idempotent.
pub(crate) fn forget_vm_unsafe_static_field_targets(vm: usize) {
    for shard in unsafe_static_field_targets().iter() {
        shard.lock().retain(|&(owner, _), _| owner != vm);
    }
}

/// First-touch classification of an offset that reached the NULL-BASE fallback
/// side store, for L1 residual R5.
///
/// A null base means an ABSOLUTE ADDRESS in the JDK's contract, and HotSpot
/// SIGSEGVs on one it cannot map. Here such an access falls through to a
/// private `static_{int,long,obj}_store` keyed by `(vm, offset)`, which invents the
/// slot and succeeds -- including `compareAndSwapInt`, which then returns
/// `true` having written nowhere any reader can see.
///
/// The population that matters is the offsets with NO explanation: not an
/// arena handle, not a synthetic offset, not a registered static field. The
/// first three are legitimate users of this store (`Thread$ThreadIdentifiers
/// .next()` mints a synthetic one). This counts and names the fourth.
///
/// Deliberately lock-free and flag-free: one relaxed `fetch_add`, and a warn
/// only on the first occurrence and then at powers of two, so the magnitude is
/// visible without a bounded-sink or a new declared flag. It is on the
/// null-base fallback branch only, which is not a hot path -- every classified
/// access returns before reaching it.
///
/// MEASURED 2026-08-29 across the whole 117-vector corpus, both modes, with a
/// denominator that has since been removed:
///
///   null-base fallback REACHED     5 warn-lines, 1 vector (RChmKeySetView)
///   of those, UNCLASSIFIED         0            0 vectors
///
/// So the fallback is genuinely exercised and every offset that reached it was
/// classifiable. The refusal this instrument exists to justify -- an
/// `IllegalArgumentException` for an unclassified offset, matching what
/// `setMemory`/`copyMemory` already do at address 0 -- is therefore free on the
/// corpus, and is STILL NOT TAKEN: the corpus does not include the three
/// definition-of-done workloads, and this fallback's sibling
/// (`objectFieldOffset1`'s synthetic mint) exists precisely for WildFly and
/// Spring Boot, which are not in it either. The instrument stays so that
/// whoever runs those workloads gets the number for free.
/// Innermost Java frame that is not one of the two `Unsafe` classes.
///
/// Used only by [`note_unsafe_side_store_offset`]'s rate-limited warn, so the
/// frame walk is paid once per power of two and never on a dispatch path.
fn ctx_caller_name(ctx: &mut dyn NativeContext) -> Option<String> {
    // `capture_stack_trace`, not `frame_class_ids`: the latter carries only a
    // ClassId, and the open question is which FIELD's offset resolved to 0 --
    // which needs the method and the bci. Captured without retaining (its own
    // doc), and only ever from the powers-of-two warn below.
    //
    // Three frames, not one. The innermost non-Unsafe frame is often a JDK
    // accessor (a VarHandle, an Atomic*) that is the same for every caller;
    // the frames above it are what distinguish call sites.
    let frames = ctx.capture_stack_trace();
    let total = frames.len();
    // REVERSED. `capture_stack_trace` returns outermost-first -- measured, see
    // this function's history: reading the front gave `main -> testFromMain ->
    // test` for every single call site, i.e. the stack's floor. Note this is
    // the OPPOSITE order from `frame_class_ids` ("innermost first" by its own
    // doc), which is why the two instruments named different callers for the
    // same event.
    let mut out: Vec<String> = vec![format!("frames={total}")];
    for f in frames.into_iter().rev() {
        // NO Unsafe filter. It used to skip `sun/misc/Unsafe` and
        // `jdk/internal/misc/Unsafe` as uninformative, but the 40-line R5
        // repro came back `frames=4` with exactly ONE printed frame -- the
        // other three were Unsafe and were hidden. `invokeCleaner` is a native
        // shim in this VM that never runs the Java cleaner, so the frame that
        // issues the access is one of the hidden ones. The method names
        // distinguish them, which is more than the skip ever removed.
        out.push(format!(
            "{}.{}+{}",
            f.class_name, f.method_name, f.byte_code_index
        ));
        if out.len() == 7 {
            break;
        }
    }
    if out.len() <= 1 {
        out.clear();
    }
    if out.is_empty() {
        // Fall back to the ClassId walk, which needs no line tables.
        // `continue` on an unresolvable frame -- the `?` that used to be here
        // propagated None out of the whole walk, so ONE bad frame blanked the
        // attribution and printed as "<no java frame>": an absence that reads
        // as a measurement.
        for cid in ctx.frame_class_ids() {
            let Some(name) = ctx.class_name_of_id(cid) else {
                continue;
            };
            if name == "sun/misc/Unsafe" || name == "jdk/internal/misc/Unsafe" {
                continue;
            }
            return Some(name);
        }
        return None;
    }
    Some(out.join(" <- "))
}

/// Returns `true` when the offset is UNCLASSIFIED, i.e. the caller must refuse.
///
/// The three classified cases -- an arena-tagged handle, a synthetic offset, a
/// registered static field -- return `false` and keep the private side store.
/// Those are the paths that unblocked `ConcurrentHashMap.initTable` and the
/// lazy-init hangs, and they are deliberately untouched.
pub(crate) fn note_unsafe_side_store_offset(
    ctx: &mut dyn NativeContext,
    offset: usize,
    site: u32,
    nargs: usize,
) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    if crate::unsafe_arena_addr_is_tagged(offset as i64)
        || crate::is_synthetic_offset(offset)
        || unsafe_static_field_target(&*ctx, offset).is_some()
    {
        // Classified: a legitimate user of the side store. Not a refusal.
        return false;
    }
    static UNCLASSIFIED: AtomicU64 = AtomicU64::new(0);
    let n = UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
    if n == 0 || n.is_power_of_two() {
        // WHO is calling. The offset alone could not say whether the caller is
        // application code, a JDK class, or one of this VM's own paths, and
        // that decides where a fix belongs -- H2 reached this 513+ times at
        // offset 0x0 in a vector that PASSES, so "refuse it" is already known
        // to be the wrong answer. Innermost frame that is not `Unsafe` itself.
        // Only evaluated on the powers-of-two warn, never on the hot path.
        let caller = ctx_caller_name(ctx);
        tracing::warn!(
            target: "cratonvm::unsafe",
            occurrence = n + 1,
            offset = format!("{offset:#x}"),
            caller = caller.as_deref().unwrap_or("<no java frame>"),
            // WHICH of the 21 null-base fallback arms. The Java caller says
            // who asked; this says which Unsafe native answered, and the two
            // together are what a fix needs. `line!()` at the call site, so it
            // costs nothing and cannot drift from the arm it names.
            site_line = site,
            // >= 3 means the arguments ARRIVED and the offset is really 0;
            // < 3 means they did not, and `offset` is 0 by ABSENCE. The two
            // are indistinguishable without this.
            nargs,
            "UNCLASSIFIED-NULL-BASE: an Unsafe access with a null base whose \
             offset is neither an arena handle, nor a synthetic offset, nor a \
             registered static field. HotSpot reads this as an absolute address \
             and faults; here it lands in a private side store, so a CAS can \
             report success having written nowhere a reader can see. L1 R5."
        );
    }
    true
}

/// The static field `offset` was registered for in the CALLING VM, if any.
pub(crate) fn unsafe_static_field_target(
    ctx: &dyn NativeContext,
    offset: usize,
) -> Option<(ClassId, usize)> {
    match latest_static_field_row((ctx.vm_identity(), offset)) {
        Some((t, _)) => Some((ClassId::new(t.class_id), t.field_index)),
        None => unscoped_static_field_target(ctx, offset, None),
    }
}

/// The static field `offset` names for a handle that carries its static base
/// (`DirectMethodHandle$StaticAccessor.staticBase`, a `VarHandle` static
/// field handle's `base`): the base's class among the offset's registrations
/// ([`unsafe_static_field_target_for_base`]), else, as before gci cl48, the
/// latest registration. These handles used to resolve the offset alone, so a
/// handle of one child-first copy of a class read and wrote the static of the
/// copy registered last.
pub(crate) fn unsafe_static_field_target_via_base(
    ctx: &dyn NativeContext,
    base: Option<ObjectRef>,
    offset: usize,
) -> Option<(ClassId, usize)> {
    if let Some(base) = base {
        if let Some(target) = unsafe_static_field_target_for_base(ctx, base, offset) {
            return Some(target);
        }
    }
    unsafe_static_field_target(ctx, offset)
}

fn unsafe_static_get(ctx: &dyn NativeContext, offset: usize) -> Option<Value> {
    let (class_id, field_index) = unsafe_static_field_target(ctx, offset)?;
    Some(ctx.get_static_field(class_id, field_index))
}

fn unsafe_static_put(ctx: &mut dyn NativeContext, offset: usize, value: Value) -> bool {
    let Some((class_id, field_index)) = unsafe_static_field_target(&*ctx, offset) else {
        return false;
    };
    ctx.set_static_field(class_id, field_index, value);
    true
}

fn unsafe_static_get_and_add_long(
    ctx: &mut dyn NativeContext,
    offset: usize,
    delta: i64,
) -> Option<i64> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    let Value::Long(old) = ctx.get_static_field(class_id, field_index) else {
        return Some(0);
    };
    ctx.set_static_field(class_id, field_index, Value::Long(old.wrapping_add(delta)));
    Some(old)
}

fn unsafe_static_get_and_set_long(
    ctx: &mut dyn NativeContext,
    offset: usize,
    value: i64,
) -> Option<i64> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    let old = match ctx.get_static_field(class_id, field_index) {
        Value::Long(old) => old,
        _ => 0,
    };
    ctx.set_static_field(class_id, field_index, Value::Long(value));
    Some(old)
}

// The above cover Long null-base statics only — Unsafe.{get,put,CAS,
// getAndAdd,getAndSet}Int/Reference/Float/Double with a null receiver still
// fell through to the never-reconciled `static_int_store`/`static_obj_store`
// side maps below, unable to see a `putstatic`-initialized seed or be seen
// by `getstatic`. Same shape as the Long fix above, extended to the other
// four value types.

fn unsafe_static_get_and_add_int(
    ctx: &mut dyn NativeContext,
    offset: usize,
    delta: i32,
) -> Option<i32> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    let Value::Int(old) = ctx.get_static_field(class_id, field_index) else {
        return Some(0);
    };
    ctx.set_static_field(class_id, field_index, Value::Int(old.wrapping_add(delta)));
    Some(old)
}

fn unsafe_static_get_and_set_int(
    ctx: &mut dyn NativeContext,
    offset: usize,
    value: i32,
) -> Option<i32> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    let old = match ctx.get_static_field(class_id, field_index) {
        Value::Int(old) => old,
        _ => 0,
    };
    ctx.set_static_field(class_id, field_index, Value::Int(value));
    Some(old)
}

fn unsafe_static_get_and_set_object(
    ctx: &mut dyn NativeContext,
    offset: usize,
    value: Option<ObjectRef>,
) -> Option<Option<ObjectRef>> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    let old = match ctx.get_static_field(class_id, field_index) {
        Value::Object(old) => old,
        _ => None,
    };
    ctx.set_static_field(class_id, field_index, Value::Object(value));
    Some(old)
}

/// CAS a registered static using `unsafe_cas_values_equal` for the
/// comparison — the same semantics `native_unsafe_cas_int`/`_object`'s
/// side-store fallback already used.
fn unsafe_static_cas(
    ctx: &mut dyn NativeContext,
    offset: usize,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    let cur = ctx.get_static_field(class_id, field_index);
    // Wave 127 (lane L4): a bit-equal float / double slot keeps its tag.
    let new_val = unsafe_retag_bits_like(&cur, new_val);
    let ok = unsafe_cas_values_equal(cur, expected);
    if ok {
        ctx.set_static_field(class_id, field_index, new_val);
    }
    Some(ok)
}

pub(crate) fn unsafe_static_field_target_for_base(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
) -> Option<(ClassId, usize)> {
    let key = (ctx.vm_identity(), offset);
    let Some((target, shared)) = latest_static_field_row(key) else {
        // A VM-less registration ([`UnscopedStaticFieldTarget`]) resolves
        // against the base mirror's class in THIS VM.
        if !UNSCOPED_STATIC_TARGETS_USED.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        // Only an offset that HAS a VM-less registration is worth the base's
        // mirror-class lookup. Without this screen, one VM-less registration
        // anywhere in the process made every Unsafe access on every other
        // offset ask for its base's class (gc-common w8 verification: a
        // synthetic-slot CAS on a plain object reached the mirror lookup).
        if !unscoped_static_field_targets().lock().contains_key(&offset) {
            return None;
        }
        let mirror_class = unsafe_base_mirror_class(ctx, obj)?;
        return unscoped_static_field_target(ctx, offset, Some(mirror_class));
    };
    let (class_id, field_index) = (ClassId::new(target.class_id), target.field_index);
    if ctx.class_id_from_mirror(obj) == Some(class_id) {
        return Some((class_id, field_index));
    }
    let obj_class = ctx.class_id_of_object(obj);
    let is_class_mirror = ctx
        .class_name_of_id(obj_class)
        .map(|name| name == "java/lang/Class")
        .unwrap_or(false);
    if is_class_mirror {
        if let Value::Int(raw) = ctx.get_field(obj, 0) {
            if raw >= 0 && ClassId::new(raw as u32) == class_id {
                return Some((class_id, field_index));
            }
        }
    }
    // gci cl48: the base mirrors another class registered under this offset
    // (a same-named class of another loader): that class's own static.
    if !shared {
        return None;
    }
    let mirror_class = unsafe_base_mirror_class(ctx, obj)?;
    let field_index = static_field_row_of_class(key, mirror_class)?;
    Some((mirror_class, field_index))
}

/// The class `obj` mirrors, when `obj` is a `java.lang.Class` mirror: the same
/// two recognitions [`unsafe_static_field_target_for_base`] applies to a
/// registered target, without one to compare against.
fn unsafe_base_mirror_class(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<ClassId> {
    if let Some(class_id) = ctx.class_id_from_mirror(obj) {
        return Some(class_id);
    }
    let obj_class = ctx.class_id_of_object(obj);
    if ctx.class_name_of_id(obj_class).as_deref() != Some("java/lang/Class") {
        return None;
    }
    match ctx.get_field(obj, 0) {
        Value::Int(raw) if raw >= 0 => Some(ClassId::new(raw as u32)),
        _ => None,
    }
}

pub(crate) fn unsafe_cas_values_equal(cur: Value, expected: Value) -> bool {
    match (cur, expected) {
        (Value::Object(a), Value::Object(b)) => a == b,
        (Value::Object(None), Value::Int(0)) | (Value::Int(0), Value::Object(None)) => true,
        (Value::Object(None), Value::Long(0)) | (Value::Long(0), Value::Object(None)) => true,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        // Interpreter round i6 wave 127, lane L4: the JDK's float / double
        // atomics are `Unsafe` int / long atomics over the raw bits
        // (`compareAndExchangeFloat` is `compareAndExchangeInt(..,
        // floatToRawIntBits(e), floatToRawIntBits(x))`), and a `float` /
        // `double` slot reads back as `Value::Float` / `Value::Double`. Those
        // compare by bits, as HotSpot's `cmpxchg` on the same 4 / 8 bytes
        // does; without these arms every such exchange missed and the callers
        // answered `expected` as a witness having stored nothing (probe
        // `L4W127FloatAtomics`). The VM's own CAS equality
        // (`vm_exec::values_equal_for_cas`) already compares cross-tag
        // primitives by bits.
        (Value::Float(f), Value::Int(b)) | (Value::Int(b), Value::Float(f)) => {
            f.to_bits() == b as u32
        }
        (Value::Double(d), Value::Long(b)) | (Value::Long(b), Value::Double(d)) => {
            d.to_bits() == b as u64
        }
        _ => false,
    }
}

/// Interpreter round i6 wave 127, lane L4: the int-width witness of an
/// `Unsafe.compareAndExchangeInt` (or an int-width read) whose slot holds
/// `old`: a `float` slot answers its raw bits, as HotSpot's 4-byte load
/// does. `None` for a value of no int-width meaning (the caller keeps its
/// pre-existing fallback).
pub(crate) fn unsafe_int_bits_of(old: &Value) -> Option<i32> {
    match old {
        Value::Int(v) => Some(*v),
        Value::Float(f) => Some(f.to_bits() as i32),
        _ => None,
    }
}

/// The long-width twin of [`unsafe_int_bits_of`]: a `double` slot answers
/// its raw bits.
pub(crate) fn unsafe_long_bits_of(old: &Value) -> Option<i64> {
    match old {
        Value::Long(v) => Some(*v),
        Value::Double(d) => Some(d.to_bits() as i64),
        _ => None,
    }
}

/// Interpreter round i6 wave 127, lane L4: the value an int- / long-width
/// `Unsafe` CAS stores into a slot that currently holds `cur`. A STATIC
/// `float` / `double` slot keeps its own tag (the raw bits re-read as the
/// floating value), since a static slot is a typed `Value` cell that
/// `getstatic` reads back as-is; any other pairing stores `new_val` unchanged.
pub(crate) fn unsafe_retag_bits_like(cur: &Value, new_val: Value) -> Value {
    match (cur, new_val) {
        (Value::Float(_), Value::Int(n)) => Value::Float(f32::from_bits(n as u32)),
        (Value::Double(_), Value::Long(n)) => Value::Double(f64::from_bits(n as u64)),
        (_, v) => v,
    }
}

#[cfg(test)]
mod i127_float_bits_cas_tests {
    use super::*;

    #[test]
    fn float_and_double_slots_compare_by_raw_bits() {
        let f = 4.0f32;
        let d = 7.0f64;
        assert!(unsafe_cas_values_equal(
            Value::Float(f),
            Value::Int(f.to_bits() as i32)
        ));
        assert!(unsafe_cas_values_equal(
            Value::Int(f.to_bits() as i32),
            Value::Float(f)
        ));
        assert!(!unsafe_cas_values_equal(
            Value::Float(-0.0),
            Value::Int(0.0f32.to_bits() as i32)
        ));
        assert!(unsafe_cas_values_equal(
            Value::Double(d),
            Value::Long(d.to_bits() as i64)
        ));
        assert!(!unsafe_cas_values_equal(
            Value::Double(d),
            Value::Long(3.5f64.to_bits() as i64)
        ));
        // A NaN payload matches its own bits.
        let nan = f32::from_bits(0x7FC0_1234);
        assert!(unsafe_cas_values_equal(
            Value::Float(nan),
            Value::Int(0x7FC0_1234)
        ));
    }

    #[test]
    fn witness_bits_and_static_retag() {
        assert_eq!(
            unsafe_int_bits_of(&Value::Float(2.5)),
            Some(2.5f32.to_bits() as i32)
        );
        assert_eq!(
            unsafe_long_bits_of(&Value::Double(3.5)),
            Some(3.5f64.to_bits() as i64)
        );
        assert_eq!(unsafe_int_bits_of(&Value::Object(None)), None);
        match unsafe_retag_bits_like(&Value::Float(1.0), Value::Int(2.5f32.to_bits() as i32)) {
            Value::Float(v) => assert_eq!(v.to_bits(), 2.5f32.to_bits()),
            other => panic!("float static lost its tag: {other:?}"),
        }
        match unsafe_retag_bits_like(&Value::Double(1.0), Value::Long(7.0f64.to_bits() as i64)) {
            Value::Double(v) => assert_eq!(v.to_bits(), 7.0f64.to_bits()),
            other => panic!("double static lost its tag: {other:?}"),
        }
        assert!(matches!(
            unsafe_retag_bits_like(&Value::Int(1), Value::Int(9)),
            Value::Int(9)
        ));
    }
}

// =====================================================================
// GC integration for the Unsafe / Class$Atomic side stores.
//
// `static_obj_store` and `synthetic_field_store` (and, until gc-common w3-b,
// the `Class$Atomic` store, now `CLASS_ATOMIC_SIDE_STORE` in lib.rs with its
// own loader-conditional root source) each hold live `ObjectRef`s that exist in NO heap slot — the value is kept
// only in the Rust-side map (this is the whole point of the synthetic-offset
// scheme: the field's real heap slot is unknown to our layout, so the load /
// CAS / store is serviced from the side channel). That makes every stored ref
// a GC root that the collector cannot reach through the heap graph.
//
// They were "missed in the original sweep" (see the migration note above) —
// migrated to parking_lot but never wired into root scanning / remapping.
// The visible failure: `java.lang.Class$Atomic.casReflectionData` stows the
// `SoftReference<ReflectionData>` for a Class mirror here; a young GC then
// reclaims that still-live SoftReference (all-zero header) and the whole
// reflection subgraph hanging off it decays — the Tomcat DoHead start/stop
// corruption flood whose FIRST victim is always a `java/lang/ref/SoftReference`
// (then cascading Method/Field/List/etc.). A moving GC instead relocates the
// ref and leaves the side-store entry dangling. Mirror the established
// `gc_scan_value_of_cache_roots` / `gc_update_value_of_cache_refs` pattern.
// =====================================================================

/// GC root scan hook — the `unsafe-side-store` row of
/// `vm/src/memory/native_roots.rs`. Reports every `ObjectRef` that VM `vm`
/// holds in an Unsafe side store so its collector keeps it live (these refs
/// live in no heap slot).
///
/// `vm` only (gc-common w7-a): both stores are keyed by VM, and another VM's
/// values are addresses in another heap -- rooting them here handed this VM's
/// collector foreign addresses, and the remap below rewrote them through this
/// VM's pointer map.
pub fn gc_scan_unsafe_side_store_roots(vm: usize, out: &mut Vec<cratonvm_types::ObjectRef>) {
    // Static-field fallback store (Unsafe.{CAS,get,put}Reference on a null
    // receiver — absolute-offset static Object fields). Sharded.
    for shard in static_obj_store().iter() {
        let map = shard.lock();
        for (&(owner, _), v) in map.iter() {
            if owner != vm {
                continue;
            }
            if let Some(o) = v {
                out.push(*o);
            }
        }
    }
    // Per-object synthetic-offset store (Value-typed; only Object values are
    // heap refs — Int/Long payloads are skipped).
    {
        let map = synthetic_field_store().lock();
        for (&(owner, _, _), v) in map.iter() {
            if owner != vm {
                continue;
            }
            if let Value::Object(Some(o)) = v {
                out.push(*o);
            }
        }
    }
    // The Class$Atomic reflectionData / annotationType / annotationData slots
    // are NOT here any more (gc-common w3-b): they are per-VM, owner-tagged and
    // loader-conditional, with their own root source (`class-atomic-slots`,
    // `gc_scan_class_atomic_side_store`). Scanning them here as well would root
    // every reflected class unconditionally again.
}

/// GC post-collection hook — the remap half of the `unsafe-side-store` row
/// (`vm/src/memory/native_roots.rs`, run from `gc::update_all_roots`). Remaps
/// VM `vm`'s side-store `ObjectRef`s through that VM's pointer map (a moving
/// GC relocates the ref; a non-moving sweep's selective promotion may tenure
/// it). Entries absent from the map did not move and stay as-is; other VMs'
/// entries are never touched (gc-common w7-a).
pub fn gc_update_unsafe_side_store_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let remap = |o: &mut cratonvm_types::ObjectRef| {
        let old_addr = o.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *o = unsafe { cratonvm_types::ObjectRef::from_raw(new_addr as *mut u8) };
        }
    };
    for shard in static_obj_store().iter() {
        let mut map = shard.lock();
        for (&(owner, _), v) in map.iter_mut() {
            if owner != vm {
                continue;
            }
            if let Some(o) = v {
                remap(o);
            }
        }
    }
    {
        let mut map = synthetic_field_store().lock();
        for (&(owner, _, _), v) in map.iter_mut() {
            if owner != vm {
                continue;
            }
            if let Value::Object(Some(o)) = v {
                remap(o);
            }
        }
    }
    // Class$Atomic slots: remapped by `gc_update_class_atomic_side_store_refs`
    // (their own root source), never here -- a second remap of the same slot
    // under a chained map would forward it twice.
}

/// Throw a real `java.lang.InstantiationException`.
///
/// There is no `RuntimeError` variant for it; `Constructor.newInstance`'s
/// abstract/interface refusal in `lang_class.rs` builds the class and throws
/// the object, and this is the same shape. The TYPE is what callers catch --
/// an `IllegalStateException` in its place is missed by every
/// `catch (InstantiationException)` in the world.
///
/// `message`: HotSpot's detail (`external_name()` for an interface or abstract
/// class; none when the mirror has no class, a primitive). Round 15 wave 4,
/// lane jdkonly6: built through `(String)`, a constructor that throws is the
/// answer (JO3-2).
fn unsafe_instantiation_exception(
    ctx: &mut dyn NativeContext,
    message: Option<&str>,
) -> MethodCallResult {
    if let Some(text) = message {
        let text = ctx.create_string_uninterned(text);
        return Err(crate::throwable_mint::construct_thrown(
            ctx,
            "java/lang/InstantiationException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(text))],
            || {
                RuntimeError::IllegalStateException {
                    message: "InstantiationException: no instances of this type exist".to_string(),
                }
                .into()
            },
        ));
    }
    if let Ok(Some(Value::Object(Some(exc)))) =
        ctx.new_object_initialized("java/lang/InstantiationException", "()V", &[])
    {
        return Err(MethodCallFailed::ExceptionThrown(exc));
    }
    Err(RuntimeError::IllegalStateException {
        message: "InstantiationException: no instances of this type exist".to_string(),
    }
    .into())
}

/// True when the frame that called this native is on the boot path.
///
/// `sun.misc.Unsafe.getUnsafe()` is `@CallerSensitive`. This resolves the
/// caller the same way the reflection gate does -- innermost Java frame,
/// trusted loader -- but WITHOUT reaching into `lang_class.rs`, which another
/// lane has open. `frame_class_ids()` is innermost-first.
///
/// It fails OPEN when no Java frame resolves: that means VM bootstrap, and the
/// reflection gate takes the same position for the same reason.
fn unsafe_caller_is_boot_path(ctx: &mut dyn NativeContext) -> bool {
    const LOADER_ID_BOOTSTRAP: i32 = 0;
    const LOADER_ID_PLATFORM: i32 = 1;
    for cid in ctx.frame_class_ids() {
        let name = match ctx.class_name_of_id(cid) {
            Some(n) => n,
            None => continue,
        };
        // The two Unsafe classes are not the caller -- `sun.misc.Unsafe`'s own
        // bytecode reaches the internal one on some paths.
        if name == "sun/misc/Unsafe" || name == "jdk/internal/misc/Unsafe" {
            continue;
        }
        let loader = ctx.loader_id_of_class(cid);
        return loader == LOADER_ID_BOOTSTRAP || loader == LOADER_ID_PLATFORM;
    }
    true
}

/// True when the `Unsafe` receiver is the `sun.misc` spelling.
///
/// `sun.misc.Unsafe` and `jdk.internal.misc.Unsafe` share every native in this
/// file but NOT every contract: the deprecated class does argument checks in
/// its own bytecode that the internal one does not. Argument 0 is the receiver
/// on every one of these instance methods, so its class is the discriminator.
fn unsafe_receiver_is_sun_misc(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    match args.first() {
        Some(Value::Object(Some(o))) => {
            let cid = ctx.class_id_of_object(*o);
            ctx.class_name_of_id(cid).as_deref() == Some("sun/misc/Unsafe")
        }
        _ => false,
    }
}

/// `new IllegalArgumentException()` with no message, as HotSpot's
/// `find_field_offset` and `Unsafe_StaticFieldBase0` throw it
/// (`THROW_0(vmSymbols::java_lang_IllegalArgumentException())`). Falls back
/// to the message form only if the constructor cannot run.
pub(crate) fn unsafe_bare_iae(ctx: &mut dyn NativeContext, fallback: &str) -> MethodCallFailed {
    match ctx.new_object_initialized("java/lang/IllegalArgumentException", "()V", &[]) {
        Ok(Some(Value::Object(Some(e)))) => MethodCallFailed::ExceptionThrown(e),
        _ => RuntimeError::IllegalArgumentException {
            message: fallback.to_string(),
        }
        .into(),
    }
}

/// `sun.misc.Unsafe`'s own refusal of a hidden or record class's field, for
/// `objectFieldOffset` / `staticFieldOffset` (`what` = `"field offset"`) and
/// `staticFieldBase` (`"base address"`): `UnsupportedOperationException("can't
/// get <what> on a hidden class: " + f)`, the hidden test first, then the
/// record one, as the JDK 25 body orders them. `None` for the internal
/// `jdk.internal.misc.Unsafe` spelling, which has no such refusal.
/// Interpreter round i4 wave 86, lane L3: the message lacked `": " + f`, the
/// record test ran first, the hidden test read the class NAME, and the two
/// static accessors did not refuse at all.
pub(crate) fn sun_misc_field_refusal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field_obj: ObjectRef,
    what: &str,
) -> Option<MethodCallFailed> {
    if !unsafe_receiver_is_sun_misc(ctx, args) {
        return None;
    }
    let (class_id, _) = crate::lang_class::field_class_and_name(ctx, field_obj)?;
    let hidden = ctx.is_class_hidden(class_id)
        || ctx
            .class_name_of_id(class_id)
            .is_some_and(|n| n.contains("/0x"));
    // `Class.isRecord()`: `Record` the direct superclass, `final`.
    let record = !hidden
        && ctx
            .superclass_of(class_id)
            .and_then(|s| ctx.class_name_of_id(s))
            .as_deref()
            == Some("java/lang/Record")
        && ctx.class_access_flags(class_id) & 0x0010 != 0;
    let kind = if hidden {
        "hidden"
    } else if record {
        "record"
    } else {
        return None;
    };
    let shown = match ctx.invoke_virtual(field_obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        Err(e) => return Some(e),
        _ => "null".to_string(),
    };
    Some(
        RuntimeError::UnsupportedOperationException {
            message: format!("can't get {what} on a {kind} class: {shown}"),
        }
        .into(),
    )
}

pub(crate) fn native_unsafe_object_field_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A NULL `Field` has no offset, and the JDK refuses rather than answering
    // one. MEASURED, HotSpot 25.0.4+7: `NullPointerException`; CratonVM
    // returned `0` in both modes, which is slot 0 of whatever receiver the
    // caller passes next -- a silent read or WRITE of an unrelated field.
    //
    // The check is missing here because it lives in the JDK's `Unsafe`
    // BYTECODE, and this native replaces that bytecode. That is the shape of
    // every defect this lane found: the retirement surface IS the JDK's
    // argument-validation layer, so a shadow that reproduces only the happy
    // path deletes the validation with it.
    // Both JDK bodies throw `new NullPointerException()`: no message (i4 wave
    // 86, lane L3; it carried this VM's words).
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    // args[0] = Unsafe this, args[1] = Field object.
    // Use read_field_meta so the slot index is read from the CratonVM
    // extra-metadata slot (the JDK `slot` field has different semantics
    // and won't match our heap offsets).
    if let Some(Value::Object(Some(field_obj))) = args.get(1) {
        // C32: Buffer.address probe — Netty's PlatformDependent0.<clinit>
        // reads Buffer.address via Unsafe.getLong(directBuffer, offset) to
        // verify a direct buffer's native address slot is accessible. If it
        // reads zero (or an uninitialised Object slot in our VM), PD0$4.run
        // returns null, and PD0.<clinit> then calls getMessage() on the null
        // "Throwable" → NPE. Use a sentinel offset (i64::MAX - 1) that
        // native_unsafe_get_long recognises and answers with a non-zero
        // long so Netty treats Buffer.address as available.
        if let Some((class_id, field_name)) =
            crate::lang_class::field_class_and_name(ctx, *field_obj)
        {
            let cname = ctx.class_name_of_id(class_id).unwrap_or_default();
            if field_name == "address" && cname == "java/nio/Buffer" {
                return Ok(Some(Value::Long(BUFFER_ADDRESS_SENTINEL as i64)));
            }
        }
        // `sun.misc.Unsafe`'s own refusals come first: its body checks the
        // declaring class before it delegates to the internal
        // `objectFieldOffset0`, whose static-field check follows.
        if let Some(refusal) = sun_misc_field_refusal(ctx, args, *field_obj, "field offset") {
            return Err(refusal);
        }
        let (is_static, _cid, slot, _desc) = crate::lang_class::read_field_meta(ctx, *field_obj);

        // A STATIC field has no object offset, and the JDK refuses rather than
        // inventing one: `Unsafe.objectFieldOffset(Field)` throws
        // `IllegalArgumentException` for a static, and `staticFieldOffset` is
        // the separate accessor for that case (registered a few lines above
        // this one). MEASURED: HotSpot 25.0.3+9 throws
        // `IllegalArgumentException` for `Integer.class.getDeclaredField(
        // "MAX_VALUE")`; CratonVM accepted it and returned an INSTANCE slot
        // index, in both modes -- `probes/UnsafeFilesSweep.java`.
        //
        // The flag was already being read here and discarded as `_is_static`,
        // so this is the check the destructuring always anticipated. Answering
        // an instance offset for a static is the dangerous direction: the
        // caller's next move is a `getInt`/`putInt` at that offset on some
        // receiver, which reads or WRITES an unrelated instance field.
        // HotSpot's `find_field_offset` throws it with NO message
        // (`THROW_0(IllegalArgumentException)`); i4 wave 86, lane L3 dropped
        // the VM's own words.
        if is_static {
            return Err(unsafe_bare_iae(ctx, "not an instance field"));
        }
        // (The record / hidden refusals of `sun.misc.Unsafe` are
        // `sun_misc_field_refusal`, above.)

        // T19.H1: if `read_field_meta` returned 0 but the Field actually
        // names an instance field of a superclass layout (the typical case
        // for AbstractMap subclasses), look it up by (declaring class,
        // field name) via the inheritance-aware path we added to
        // objectFieldOffset1. This is the fallback that unblocks the
        // ConcurrentHashMap.initTable livelock — the synthetic Field
        // emitted by `getDeclaredField` for sizeCtl had rj_slot=0 because
        // its populator only set the JDK `slot` field (which is an
        // opaque index, not our heap slot), and the fallback chain above
        // only reads two well-known slots. The inheritance walk below
        // guarantees the returned offset is the real heap slot.
        let effective_slot = if let Some((class_id, field_name)) =
            crate::lang_class::field_class_and_name(ctx, *field_obj)
        {
            let cname = ctx.class_name_of_id(class_id).unwrap_or_default();
            let resolved = ctx.resolve_field_index(&cname, &field_name).or_else(|| {
                let mut cid_opt = Some(class_id);
                while let Some(cid) = cid_opt {
                    let fields = ctx.declared_fields(cid);
                    if let Some(f) = fields.iter().find(|f| !f.is_static && f.name == field_name) {
                        return Some(f.slot_index);
                    }
                    cid_opt = ctx.superclass_of(cid);
                }
                None
            });
            if let Some(s) = resolved {
                if s != slot {
                    static FIXED: std::sync::OnceLock<
                        std::sync::Mutex<std::collections::HashSet<(String, String)>>,
                    > = std::sync::OnceLock::new();
                    let seen = FIXED
                        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
                    let mut g = seen.lock().unwrap_or_else(|e| e.into_inner());
                    if g.insert((cname.clone(), field_name.clone())) {
                        tracing::info!(
                            target: "cratonvm::unsafe",
                            "objectFieldOffset(Field): class={cname:?} field={field_name:?} \
                             rj_slot={slot} → resolved by name to slot={s}"
                        );
                    }
                }
                s
            } else {
                slot
            }
        } else {
            slot
        };

        // JIT round 20 wave 8c: a byte offset under `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`.
        let encoded = unsafe_encode_field_offset(ctx.unsafe_byte_field_offsets(), effective_slot);
        return Ok(Some(Value::Long(encoded as i64)));
    }
    Ok(Some(Value::Long(0)))
}

pub(crate) fn native_unsafe_static_field_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Null and instance-field refusals, both MEASURED against HotSpot
    // 25.0.4+7 and both missing here: a null answered `0` and an INSTANCE
    // field was quietly forwarded to `objectFieldOffset`, so a caller that
    // asked the wrong accessor got a plausible offset in the wrong address
    // space. This is the exact mirror of the `objectFieldOffset(static)`
    // defect fixed on 2026-08-26 -- that fix closed one polarity of a
    // two-sided confusion and left the other open.
    // i4 wave 86, lane L3: HotSpot's texts -- a bare NPE (`new
    // NullPointerException()`), `sun.misc`'s hidden/record refusal, and a bare
    // IAE (`find_field_offset`) -- in the JDK's order.
    let Some(Value::Object(Some(field_obj))) = args.get(1) else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    if let Some(refusal) = sun_misc_field_refusal(ctx, args, *field_obj, "field offset") {
        return Err(refusal);
    }
    let (is_static, meta_class_id, meta_slot, _desc) =
        crate::lang_class::read_field_meta(ctx, *field_obj);
    if !is_static {
        return Err(unsafe_bare_iae(ctx, "not a static field"));
    }

    let (class_id, field_name) = crate::lang_class::field_class_and_name(ctx, *field_obj)
        .unwrap_or_else(|| (meta_class_id, format!("slot#{meta_slot}")));
    let field_index = ctx
        .static_field_index_by_name(class_id, &field_name)
        .unwrap_or(meta_slot);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    // Wave 103, L2: keyed by the class too (see `static_field_synthetic_offset`).
    let offset = crate::static_field_synthetic_offset(&class_name, &field_name, class_id);
    remember_unsafe_static_field_offset(ctx.vm_identity(), offset, class_id, field_index);
    // JIT round 20 wave 11: the scaled static byte offset under
    // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` (the minted one otherwise).
    let offset = unsafe_static_offset_answer(&*ctx, offset, class_id, field_index);
    Ok(Some(Value::Long(offset as i64)))
}

/// The refusal both `arrayBaseOffset` and `arrayIndexScale` owe a class that
/// has no elements — `Some(err)` when `args[1]` is a positively-identified
/// non-array, `None` otherwise.
///
/// # What HotSpot throws, and why this throws it too
///
/// HotSpot's refusal is itself broken. `Unsafe_ArrayBaseOffset0` does
/// `THROW_0(vmSymbols::java_lang_InvalidClassException())`, and that symbol
/// names `java/lang/InvalidClassException` — a class that does not exist,
/// since `InvalidClassException` lives in **`java.io`**. The throw fails to
/// link and the caller gets a `NoClassDefFoundError` naming the missing class.
/// Measured on HotSpot 25.0.4+7, all four doors, every non-array input:
///
/// ```text
///   sun.misc / jdk.internal.misc  ×  arrayBaseOffset / arrayIndexScale
///     -> java.lang.NoClassDefFoundError: java/lang/InvalidClassException
/// ```
///
/// Until round 15 wave 4 this VM threw a `java.io.InvalidClassException`
/// instead ("the intent, delivered through a class that links"), on the
/// argument that matching would reproduce a JDK bug. `AGENTS.md` has since
/// settled it: both modes target HotSpot 25's Java-visible behaviour byte for
/// byte, exception types and messages included, so this throws exactly the
/// `NoClassDefFoundError` above. The day the JDK fixes its symbol, this
/// follows the new oracle.
///
/// The alternative, answering a number, is the direction
/// `unsafe-objectfieldoffset-accepted-a-static-...` calls the dangerous one:
/// `16`/`1` gave a caller a plausible-looking basis for address arithmetic
/// over a class with no elements, silently. `arrayIndexScale` was moved to `0`
/// on 2026-08-29 because `if (scale == 0) throw` is the guard callers write,
/// but `arrayBaseOffset` has no such sentinel — `0` is a perfectly ordinary
/// base offset — so the two siblings could not be made safe by the same
/// constant. A refusal makes them agree, and it is loud.
///
/// # Why an unreadable mirror is NOT refused
///
/// The check fires only on a name that was positively read and positively does
/// not begin with `[`. A mirror whose name this VM cannot read keeps the
/// legacy answer: the refusal is meant to catch a caller asking about
/// `String.class`, not to punish an internal path holding a mirror shape the
/// name reader does not model.
fn refuse_non_array_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> Option<MethodCallFailed> {
    let Some(Value::Object(Some(mirror))) = args.get(1) else {
        return None;
    };
    let name = crate::lang_class::mirror_class_name(ctx, *mirror)?;
    if name.is_empty() || name.starts_with('[') {
        return None;
    }
    // Round 15 wave 4, lane jdkonly4
    // (`r15w3-jdkonly3-unsafe-non-array-refusal-is-not-hotspots-FIXED-20260930.md`):
    // THE OWNER DECISION. `AGENTS.md` binds both modes to HotSpot 25's
    // Java-visible behaviour byte for byte, exception types and messages
    // included, so the "judgement call" above is closed in HotSpot's favour:
    // throw what HotSpot throws, `java.lang.NoClassDefFoundError` whose
    // message is the missing symbol in slash form. Built through
    // `NoClassDefFoundError(String)` by the class-loader funnel (LT-4: the
    // constructor, a fresh message; its flat fallback when no constructor can
    // run), which is how every other native NCDFE of this VM is built. It
    // was a constructed `java.io.InvalidClassException("Unsafe.<m>: <name> is
    // not an array class")` (round 15 wave 3) and a bare mint before that.
    // `CRATONVM_UNSAFE_NON_ARRAY_NCDFE=0` restores the wave 3 exception (a
    // kill switch for the type change; read in both modes).
    // `(class, stub-layout field count, message)`; the counts are what the
    // funnel's other callers and the wave 3 mint asked for.
    let (class, num_fields, message) =
        if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_UNSAFE_NON_ARRAY_NCDFE") {
            (
                "java/lang/NoClassDefFoundError",
                1,
                "java/lang/InvalidClassException".to_string(),
            )
        } else {
            (
                "java/io/InvalidClassException",
                2,
                format!(
                    "Unsafe.{method}: {} is not an array class",
                    name.replace('/', ".")
                ),
            )
        };
    match crate::jboss_module_loader::alloc_single_message_exception(
        ctx, class, num_fields, &message,
    ) {
        Ok(exc) => Some(MethodCallFailed::ExceptionThrown(exc)),
        // The constructor ran and threw (JO3-2): that throwable is the answer.
        Err(thrown @ MethodCallFailed::ExceptionThrown(_)) => Some(thrown),
        // The refusal could not be built at all. Degrade to the legacy number
        // rather than propagate an allocation failure out of a method that
        // never threw one before — the same DEGRADE contract the locale SPI
        // arms in `locale_bootstrap` use, and for the same reason: an arm
        // added to improve an answer must not be able to introduce a failure
        // mode that was not there before.
        Err(_) => None,
    }
}

/// R3 of `r15w4-jdkonly4-builder-review-findings-CLOSED-20260930.md` (round 15 wave 5,
/// lane jdkonly7): the `NullPointerException` of `arrayBaseOffset(null)` /
/// `arrayIndexScale(null)` has NO message on HotSpot 25 -- `Unsafe`'s own
/// `if (arrayClass == null) throw new NullPointerException()`, an explicit
/// NPE, which gets no helpful text (measured: `R15Jdkonly4UnsafeNonArray` row
/// `N`, HotSpot `msg=80`, `--compatible` `msg=0`). `legacy` is this VM's old
/// text, kept for `CRATONVM_UNSAFE_NULL_CLASS_NPE_BARE=0` (read on the refusal
/// only).
fn unsafe_null_class_npe_message(legacy: &str) -> Option<String> {
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_UNSAFE_NULL_CLASS_NPE_BARE") {
        None
    } else {
        Some(legacy.to_string())
    }
}

/// `Unsafe.arrayBaseOffset(Class)` — byte offset of element 0 from the
/// array object's base. Real HotSpot returns 16 on 64-bit (object header
/// size). JCTools `UnsafeRefArrayAccess.<clinit>` and similar consumers
/// use this value as-is for pointer arithmetic, but nothing in our VM
/// actually dereferences the computed address; we only need the static
/// initializers to accept the value. Returning 16 matches the 64-bit
/// HotSpot layout and keeps JCTools happy.
///
/// A class that is not an array is refused — see [`refuse_non_array_class`].
pub(crate) fn native_unsafe_array_base_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // MEASURED, HotSpot 25.0.4+7: `arrayBaseOffset(null)` is a
    // `NullPointerException`. CratonVM answered 16 -- a plausible base for a
    // class that was never named. Note the asymmetry this closes: the
    // `jdk.internal.misc` spelling ALREADY threw, because that registration
    // shadows nothing on this image and the JDK's own bytecode ran instead.
    // Three of four sibling doors were wrong and the fourth was right for a
    // reason that had nothing to do with the check.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: unsafe_null_class_npe_message("Unsafe.arrayBaseOffset: null class"),
        }
        .into());
    }
    if let Some(refusal) = refuse_non_array_class(ctx, args, "arrayBaseOffset") {
        return Err(refusal);
    }
    Ok(Some(Value::Int(16)))
}

/// `Unsafe.arrayIndexScale(Class)` — element size in bytes. JCTools'
/// `UnsafeRefArrayAccess.<clinit>` computes `pointer size` from the scale
/// of `Object[]` and throws `IllegalStateException: Unknown pointer size`
/// for anything other than 4 or 8. We derive the scale from the array
/// element type encoded in the Class mirror's name (JVMS descriptor form,
/// e.g. `[I`, `[J`, `[Ljava/lang/Object;`). Scale matches HotSpot's
/// 64-bit layout without compressed oops:
///   `[Z` / `[B` → 1
///   `[S` / `[C` → 2
///   `[I` / `[F` → 4
///   `[J` / `[D` / `[L...;` / `[[...` → 8
///
/// A class that is not an array is refused — see [`refuse_non_array_class`].
/// `array_index_scale_for_name`'s `0` for a non-array is still the answer for
/// a mirror this VM cannot name, and stays the documented contract of that
/// helper; the refusal here sits in front of it and is the one a caller who
/// names `String.class` actually gets.
pub(crate) fn native_unsafe_array_index_scale(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Unsafe this, args[1] = Class mirror of the array type.
    // MEASURED, HotSpot 25.0.4+7: a null class is a `NullPointerException`,
    // not the catch-all scale of 1. See `native_unsafe_array_base_offset`.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: unsafe_null_class_npe_message("Unsafe.arrayIndexScale: null class"),
        }
        .into());
    }
    if let Some(refusal) = refuse_non_array_class(ctx, args, "arrayIndexScale") {
        return Err(refusal);
    }
    let scale = match args.get(1) {
        Some(Value::Object(Some(mirror))) => {
            let name = crate::lang_class::mirror_class_name(ctx, *mirror).unwrap_or_default();
            array_index_scale_for_name(&name)
        }
        _ => 1,
    };
    Ok(Some(Value::Int(scale)))
}

/// The atomic operation [`unsafe_arena_atomic`] applies.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArenaAtomic {
    /// `compareAndSet*`: answers the boolean as `Int(1/0)`.
    Cas,
    /// `compareAndExchange*`: answers the witnessed value.
    Cae,
    /// `getAndAdd*`: answers the previous value.
    Add,
    /// `getAndSet*`: answers the previous value.
    Set,
}

/// SBR3 lane ffm: the atomics of `Unsafe` over `Unsafe.allocateMemory` memory
/// (`base == null`, `offset` a live arena handle), on the REAL bytes.
///
/// Before this, every atomic with a null base and an arena handle fell to the
/// private per-offset side store (`note_unsafe_side_store_offset` classifies a
/// handle as "legitimate"): a `compareAndSet` over the memory `MemorySegment.
/// ofConfined().allocate(..)` hands out compared against a side-store `0`
/// instead of the bytes the plain `get`/`put` wrote, so a real
/// `VarHandleSegmentAsInts.compareAndSet` answered `false` and `getAndAdd`
/// returned `0` without adding (HotSpot answers over the memory). `None` when
/// the call is not that shape (a non-null base, an untagged offset, a freed
/// handle, a block shorter than `width`): the caller keeps its old behaviour.
/// `width` is 1, 2, 4 or 8; `args` is `[unsafe, base, offset, a, b]` (`a`/`b`
/// are `expected`/`new` for `Cas`/`Cae`, `a` is the operand for `Add`/`Set`).
pub(crate) fn unsafe_arena_atomic(args: &[Value], op: ArenaAtomic, width: usize) -> Option<Value> {
    use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering::SeqCst};
    if matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return None;
    }
    let offset = unsafe_offset(args, 2);
    let addr = offset as i64;
    if !unsafe_arena_addr_is_tagged(addr) {
        return None;
    }
    let ptr = unsafe_arena_real_ptr_bounded(addr, width)?;
    let raw = |i: usize| -> u64 {
        match args.get(i) {
            Some(Value::Long(v)) => *v as u64,
            Some(Value::Int(v)) => *v as i64 as u64,
            _ => 0,
        }
    };
    let mask: u64 = if width == 8 {
        u64::MAX
    } else {
        (1u64 << (width * 8)) - 1
    };
    let a = raw(3) & mask;
    let b = raw(4) & mask;
    let aligned = (ptr as usize) % width == 0;
    // (previous value, whether a compare matched)
    let (old, ok) = if aligned {
        // SAFETY: `ptr` is a live arena block's pointer, good for `width`
        // bytes (`unsafe_arena_real_ptr_bounded`) and `width`-aligned.
        unsafe {
            macro_rules! rmw {
                ($at:ty, $t:ty) => {{
                    let at = <$at>::from_ptr(ptr as *mut $t);
                    match op {
                        ArenaAtomic::Cas | ArenaAtomic::Cae => {
                            match at.compare_exchange(a as $t, b as $t, SeqCst, SeqCst) {
                                Ok(v) => (v as u64, true),
                                Err(v) => (v as u64, false),
                            }
                        }
                        ArenaAtomic::Add => (at.fetch_add(a as $t, SeqCst) as u64, true),
                        ArenaAtomic::Set => (at.swap(a as $t, SeqCst) as u64, true),
                    }
                }};
            }
            match width {
                1 => rmw!(AtomicU8, u8),
                2 => rmw!(AtomicU16, u16),
                4 => rmw!(AtomicU32, u32),
                8 => rmw!(AtomicU64, u64),
                _ => return None,
            }
        }
    } else {
        // A misaligned word: one process-wide lock keeps the pair of accesses
        // atomic against every other misaligned atomic (plain stores are not
        // synchronised with it, as on a CPU without a locked misaligned RMW).
        static MISALIGNED: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = MISALIGNED.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = [0u8; 8];
        // SAFETY: `ptr` is good for `width` bytes (see above).
        unsafe { std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), width) };
        let cur = u64::from_le_bytes(buf) & mask;
        let (new, ok) = match op {
            ArenaAtomic::Cas | ArenaAtomic::Cae => (b, cur == a),
            ArenaAtomic::Add => (cur.wrapping_add(a) & mask, true),
            ArenaAtomic::Set => (a, true),
        };
        if ok {
            let nb = new.to_le_bytes();
            // SAFETY: as above.
            unsafe { std::ptr::copy_nonoverlapping(nb.as_ptr(), ptr, width) };
        }
        (cur, ok)
    };
    Some(match op {
        ArenaAtomic::Cas => Value::Int(i32::from(ok)),
        _ => match width {
            8 => Value::Long(old as i64),
            4 => Value::Int(old as u32 as i32),
            2 => Value::Int(old as u16 as i16 as i32),
            _ => Value::Int(old as u8 as i8 as i32),
        },
    })
}

/// SBR3 lane ffm: a volatile load of `width` (4 or 8) bytes at a null-base
/// arena handle, from the REAL bytes. The JDK's own `getAndAddInt` /
/// `getAndSetInt` / `getAndBitwise*` are bytecode loops over `getIntVolatile` +
/// `compareAndSetInt` (under `--jdk-only` no native shadows them), and the
/// volatile loads read the private side store while the CAS now works on the
/// bytes: the loop's compare never matched and it spun forever
/// (`sun.misc.Unsafe.getAndAddInt(null, allocateMemory(..), 3)`).
fn unsafe_arena_volatile_load(args: &[Value], width: usize) -> Option<u64> {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
    if matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return None;
    }
    let addr = unsafe_offset(args, 2) as i64;
    if !unsafe_arena_addr_is_tagged(addr) {
        return None;
    }
    let ptr = unsafe_arena_real_ptr_bounded(addr, width)?;
    let aligned = (ptr as usize) % width == 0;
    // SAFETY: `ptr` is a live arena block's pointer good for `width` bytes.
    Some(unsafe {
        match (width, aligned) {
            (4, true) => u64::from(AtomicU32::from_ptr(ptr as *mut u32).load(SeqCst)),
            (4, false) => u64::from((ptr as *const u32).read_unaligned()),
            (_, true) => AtomicU64::from_ptr(ptr as *mut u64).load(SeqCst),
            (_, false) => (ptr as *const u64).read_unaligned(),
        }
    })
}

/// The store half of [`unsafe_arena_volatile_load`]; `true` when it wrote.
fn unsafe_arena_volatile_store(args: &[Value], width: usize, raw: u64) -> bool {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
    if matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return false;
    }
    let addr = unsafe_offset(args, 2) as i64;
    if !unsafe_arena_addr_is_tagged(addr) {
        return false;
    }
    let Some(ptr) = unsafe_arena_real_ptr_bounded(addr, width) else {
        return false;
    };
    let aligned = (ptr as usize) % width == 0;
    // SAFETY: as in `unsafe_arena_volatile_load`.
    unsafe {
        match (width, aligned) {
            (4, true) => AtomicU32::from_ptr(ptr as *mut u32).store(raw as u32, SeqCst),
            (4, false) => (ptr as *mut u32).write_unaligned(raw as u32),
            (_, true) => AtomicU64::from_ptr(ptr as *mut u64).store(raw, SeqCst),
            (_, false) => (ptr as *mut u64).write_unaligned(raw),
        }
    }
    true
}

/// `Unsafe.getIntVolatile` (int width only: the byte/short/char/float
/// registrations share the unwrapped native).
pub(crate) fn native_unsafe_get_int_volatile_arena(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(raw) = unsafe_arena_volatile_load(args, 4) {
        return Ok(Some(Value::Int(raw as u32 as i32)));
    }
    if let Some(raw) = unsafe_wide_raw_load(ctx, args, cratonvm_types::ArrayElementType::Int, 4) {
        return Ok(Some(Value::Int(raw as u32 as i32)));
    }
    // Interpreter round i6 wave 127, lane L4: `getIntVolatile` AT a `float`
    // field's own offset is the field's bits, as `getInt` already answers
    // (JIT round 20 wave 7). The JDK's `getAndSetFloat` / `getAndAddFloat`
    // loop on exactly this read (`intBitsToFloat(getIntVolatile(o, off))`),
    // and the slot's `Value::Float` reached the `I` return, so the old value
    // read as 0.0 and 0.0 was what the CAS compared and the update summed
    // (probe `L4W127FloatAtomics`). The unwrapped native stays as-is: the
    // `getFloatVolatile` registrations share it and want the `Value::Float`.
    Ok(native_unsafe_get_int_volatile(ctx, args)?.map(|v| match v {
        Value::Float(f) => Value::Int(f.to_bits() as i32),
        // A narrower read of a `double` field is its low bytes, as `getInt`.
        Value::Double(d) => Value::Int(d.to_bits() as i32),
        other => other,
    }))
}

/// `Unsafe.putIntVolatile`; see [`native_unsafe_get_int_volatile_arena`].
pub(crate) fn native_unsafe_put_int_volatile_arena(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Int(v)) = args.get(3) {
        if unsafe_arena_volatile_store(args, 4, *v as u32 as u64) {
            return Ok(None);
        }
    }
    if unsafe_wide_raw_store(ctx, args, cratonvm_types::ArrayElementType::Int, 4) {
        return Ok(None);
    }
    native_unsafe_put_int_volatile(ctx, args)
}

/// `Unsafe.getLongVolatile`; see [`native_unsafe_get_int_volatile_arena`].
pub(crate) fn native_unsafe_get_long_volatile_arena(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(raw) = unsafe_arena_volatile_load(args, 8) {
        return Ok(Some(Value::Long(raw as i64)));
    }
    if let Some(raw) = unsafe_wide_raw_load(ctx, args, cratonvm_types::ArrayElementType::Long, 8) {
        return Ok(Some(Value::Long(raw as i64)));
    }
    // Wave 127 (lane L4): a `double` field's bits, as `getLong` answers; see
    // `native_unsafe_get_int_volatile_arena` (`getAndSetDouble` /
    // `getAndAddDouble`). `getDoubleVolatile` keeps the unwrapped native.
    Ok(native_unsafe_get_long_volatile(ctx, args)?.map(|v| match v {
        Value::Double(d) => Value::Long(d.to_bits() as i64),
        other => other,
    }))
}

/// `Unsafe.putLongVolatile`; see [`native_unsafe_get_int_volatile_arena`].
pub(crate) fn native_unsafe_put_long_volatile_arena(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Long(v)) = args.get(3) {
        if unsafe_arena_volatile_store(args, 8, *v as u64) {
            return Ok(None);
        }
    }
    if unsafe_wide_raw_store(ctx, args, cratonvm_types::ArrayElementType::Long, 8) {
        return Ok(None);
    }
    native_unsafe_put_long_volatile(ctx, args)
}

/// Round nio20 wave 2, lane L4 (proposal L2-4 of
/// `docs/internal/proposals/jit/jit-r20-nio-proposals-L2.md`): an int- or long-width
/// `Unsafe` access whose base is a primitive array of ANOTHER element type
/// (`getLongVolatile(byte[], ..)` from a byte-array view `VarHandle`,
/// `compareAndSetInt(float[], ..)` from `compareAndSetFloat`,
/// `putIntVolatile(long[], ..)`), or that straddles two elements. HotSpot
/// reads and writes the raw little-endian bytes; the width-matched volatile and
/// CAS natives decoded `offset` as an index of the array's own element type and
/// read / compared / stored ONE element (a byte, a float `Value`), so the plain
/// accessors (`*_mb`, already raw) and the volatile / atomic ones disagreed.
///
/// `Some((array, offset))` for that shape; `None` keeps the caller's
/// element-typed path (arena handles, fields, a width-matched aligned element).
fn unsafe_wide_raw_target(
    ctx: &dyn NativeContext,
    args: &[Value],
    natural: cratonvm_types::ArrayElementType,
    width: usize,
) -> Option<(cratonvm_types::ObjectRef, usize)> {
    let obj = unsafe_obj(args, 1)?;
    let offset = unsafe_offset(args, 2);
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return None;
    }
    unsafe_array_access_is_raw(ctx, obj, natural, offset, width).then_some((obj, offset))
}

/// The little-endian raw bits of `width` (4 or 8) bytes for a
/// [`unsafe_wide_raw_target`] load; `None` when out of bounds.
fn unsafe_wide_raw_load(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    natural: cratonvm_types::ArrayElementType,
    width: usize,
) -> Option<u64> {
    let (obj, off) = unsafe_wide_raw_target(ctx, args, natural, width)?;
    // A naturally aligned access is one hardware-atomic load of the payload
    // (single-copy atomic against compiled code's stores, as HotSpot's).
    if let Some((cur, _)) = unsafe_wide_payload_atomic(ctx, obj, off, width, &|_| None) {
        return Some(cur);
    }
    let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
    if !unsafe_read_bytes_into(ctx, obj, off, width, &mut b) {
        return None;
    }
    Some(u64::from_le_bytes(b))
}

/// The store half of [`unsafe_wide_raw_load`]: `args[3]`'s raw bits written as
/// `width` little-endian bytes. `true` when it wrote.
fn unsafe_wide_raw_store(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    natural: cratonvm_types::ArrayElementType,
    width: usize,
) -> bool {
    let Some((obj, off)) = unsafe_wide_raw_target(ctx, args, natural, width) else {
        return false;
    };
    let bits = unsafe_wide_arg_bits(args, 3, width);
    if unsafe_wide_payload_atomic(ctx, obj, off, width, &|_| Some(bits)).is_some() {
        return true;
    }
    unsafe_array_write_bytes(ctx, obj, off, &bits.to_le_bytes()[..width])
}

/// The hardware-atomic arm of every raw wide access:
/// `NativeHeapAccess::primitive_array_atomic_update` at payload byte
/// `off - ABASE`. `None` (misaligned, no flat payload, out of range) sends the
/// caller to its byte-copy / locked fallback.
fn unsafe_wide_payload_atomic(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    off: usize,
    width: usize,
    f: &dyn Fn(u64) -> Option<u64>,
) -> Option<(u64, bool)> {
    const ABASE: usize = 16;
    let rel = off.checked_sub(ABASE)?;
    ctx.primitive_array_atomic_update(obj, rel, width, f)
}

/// `args[i]` as raw bits masked to `width` bytes (an `Int`, `Long`, `Float` or
/// `Double` operand).
fn unsafe_wide_arg_bits(args: &[Value], i: usize, width: usize) -> u64 {
    let bits = match args.get(i) {
        Some(Value::Int(v)) => *v as u32 as u64,
        Some(Value::Long(v)) => *v as u64,
        Some(Value::Float(f)) => u64::from(f.to_bits()),
        Some(Value::Double(d)) => d.to_bits(),
        _ => 0,
    };
    if width >= 8 {
        bits
    } else {
        bits & ((1u64 << (width * 8)) - 1)
    }
}

/// A compare-and-set of `width` raw bytes at `off` of a primitive array:
/// `Some((witnessed, swapped))`, `None` when out of bounds. One process-wide
/// lock makes it atomic against itself (and so against every other raw CAS,
/// including the JDK's `getAndAdd*` loops over it), not against a concurrent
/// element-typed store into the same bytes -- the [`SubwordLane`] straddle
/// rule. Not compatibility state: a lock, no data.
fn unsafe_wide_raw_cas(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    off: usize,
    width: usize,
    expected: u64,
    new: u64,
) -> Option<(u64, bool)> {
    unsafe_wide_raw_update(ctx, obj, off, width, &|cur| {
        (cur == expected).then_some(new)
    })
}

/// Interpreter round i6 wave 127, lane L4: the raw-bytes arm of an int- or
/// long-width `Unsafe.compareAndExchange*` (args `[unsafe, base, offset,
/// expected, new]`), for the natives outside this module that lacked it
/// (`unsafe_natives::cae_int_impl` / `cae_long_impl`, the registrations that
/// win): `compareAndExchangeFloat` over a `float[]` is
/// `compareAndExchangeInt` with the raw bits, and the element-typed path
/// compared those bits against a `Value::Float` element. `Some(witness bits)`
/// when the shape is a raw one ([`unsafe_wide_raw_target`]), else `None`.
pub(crate) fn unsafe_wide_raw_cae(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    natural: cratonvm_types::ArrayElementType,
    width: usize,
) -> Option<u64> {
    let (obj, off) = unsafe_wide_raw_target(&*ctx, args, natural, width)?;
    let (ex, nv) = (
        unsafe_wide_arg_bits(args, 3, width),
        unsafe_wide_arg_bits(args, 4, width),
    );
    unsafe_wide_raw_cas(ctx, obj, off, width, ex, nv).map(|(cur, _)| cur)
}

/// `getAndAdd` / `getAndSet` over raw bytes: replaces the `width` bytes with
/// `f(current)` (masked to `width`) and answers the previous value; `None`
/// when out of bounds. Same lock as [`unsafe_wide_raw_cas`].
fn unsafe_wide_raw_rmw(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    off: usize,
    width: usize,
    f: &dyn Fn(u64) -> u64,
) -> Option<u64> {
    unsafe_wide_raw_update(ctx, obj, off, width, &|cur| Some(f(cur))).map(|(old, _)| old)
}

/// The one locked read-compute-write both raw atomics share: `f(current)`
/// `None` leaves the bytes. Answers `(witnessed, wrote)`.
fn unsafe_wide_raw_update(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    off: usize,
    width: usize,
    f: &dyn Fn(u64) -> Option<u64>,
) -> Option<(u64, bool)> {
    // Naturally aligned (the only shape a byte-array / ByteBuffer view
    // `VarHandle` admits for an atomic mode -- it throws IllegalStateException
    // on a misaligned one before reaching `Unsafe` -- and every `float[]` /
    // `double[]` element): a real `lock cmpxchg` on the payload, atomic against
    // compiled code's CAS / xadd on the same bytes.
    if let Some(r) = unsafe_wide_payload_atomic(ctx, obj, off, width, f) {
        return Some(r);
    }
    // nio21 wave 6 L3: on x86-64 the arm above now serves a MISALIGNED raw
    // `Unsafe` access too, as HotSpot's `lock cmpxchg` at that address. What
    // reaches here is a payload with no flat address, a split lock (an access
    // crossing a cache line, unless `CRATONVM_UNSAFE_SPLIT_LOCK_CAS=0`),
    // another architecture, or `CRATONVM_UNSAFE_MISALIGNED_HW_CAS=0`: atomic
    // only against other calls of this path. Not compatibility state: a lock,
    // no data.
    static WIDE_RAW: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = WIDE_RAW.lock().unwrap_or_else(|e| e.into_inner());
    let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
    if !unsafe_read_bytes_into(ctx, obj, off, width, &mut b) {
        return None;
    }
    let cur = u64::from_le_bytes(b);
    let Some(new) = f(cur) else {
        return Some((cur, false));
    };
    if !unsafe_array_write_bytes(ctx, obj, off, &new.to_le_bytes()[..width]) {
        return None;
    }
    Some((cur, true))
}

/// SBR3 lane unsafecas: a BYTE LANE of a heap array, the target of a byte/short
/// `Unsafe` access that is not the array's element width: a byte or short inside
/// an `int[]`/`long[]`/`short[]`/`char[]` element, or a short across two
/// `byte[]`/`boolean[]` elements. HotSpot names it by the byte offset
/// `ABASE + n` (little-endian within an element).
///
/// This VM's heap FIELDS are one slot each (a byte field is already its own
/// `Int` cell, so the int-width paths are exact for it) but its arrays are typed
/// element cells addressed `ABASE + index * scale`: the int-width natives read
/// `offset - ABASE` as an element index for the ELEMENT's width and stored the
/// byte into the wrong lane (`compareAndSetByte(int[], base + 1, 0, 0x7f)` left
/// `0x7f`, HotSpot `0x7f00`), and `getByteVolatile` on raw memory read a side
/// store, which made the JDK's `getAndSetByte` loop (volatile load + CAS) spin.
#[derive(Clone, Copy)]
struct SubwordLane {
    obj: cratonvm_types::ObjectRef,
    elem: cratonvm_types::ArrayElementType,
    /// Element size in bytes.
    scale: usize,
    /// Access width in bytes (1 or 2).
    width: usize,
    /// Bytes from element 0.
    byte_off: usize,
}

impl SubwordLane {
    /// `None` when the call is not that shape: not a heap array, a width other
    /// than 1 or 2, an element of the access width (the width-matched path is
    /// exact), a float/double/reference element, an offset below `ABASE` or past
    /// the end, or a lane straddling two wide elements.
    fn locate(
        ctx: &dyn NativeContext,
        obj: cratonvm_types::ObjectRef,
        offset: usize,
        width: usize,
    ) -> Option<Self> {
        use cratonvm_types::ArrayElementType as E;
        if !(1..=2).contains(&width) || ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
            return None;
        }
        let elem = ctx.heap_element_type_of(obj);
        let scale = match elem {
            E::Boolean | E::Byte => 1usize,
            E::Char | E::Short => 2,
            E::Int => 4,
            E::Long => 8,
            E::Float | E::Double | E::Reference => return None,
        };
        if scale == width {
            return None;
        }
        // ABASE is 16 (`native_unsafe_array_base_offset`).
        const ABASE: usize = 16;
        let byte_off = offset.checked_sub(ABASE)?;
        if byte_off.checked_add(width)? > ctx.array_length(obj).checked_mul(scale)? {
            return None;
        }
        if scale > width && (byte_off % scale) + width > scale {
            return None;
        }
        Some(SubwordLane {
            obj,
            elem,
            scale,
            width,
            byte_off,
        })
    }

    fn mask(&self) -> u64 {
        (1u64 << (self.width * 8)) - 1
    }

    fn elem_bits(&self, v: &Value) -> Option<u64> {
        match v {
            Value::Int(i) => Some(*i as u32 as u64 & ((1u64 << (self.scale * 8)) - 1)),
            Value::Long(l) => Some(*l as u64),
            _ => None,
        }
    }

    /// The lane's value, zero-extended.
    fn load(&self, ctx: &dyn NativeContext) -> Option<u64> {
        if self.scale > self.width {
            let bits =
                self.elem_bits(&ctx.get_array_element(self.obj, self.byte_off / self.scale))?;
            return Some((bits >> ((self.byte_off % self.scale) * 8)) & self.mask());
        }
        let mut cur = 0u64;
        for i in 0..self.width {
            let b = self.elem_bits(&ctx.get_array_element(self.obj, self.byte_off + i))? & 0xff;
            cur |= b << (8 * i);
        }
        Some(cur)
    }

    /// Atomically replace the lane with `f(current)` (`None` = leave it). Returns
    /// the value witnessed and whether `f` wrote. Over a wide element the swap is
    /// a CAS loop on the whole element (atomic against every other element CAS);
    /// a short over two byte elements takes one process-wide lock (atomic against
    /// itself, not against a concurrent byte store, as for a misaligned word on a
    /// CPU without a locked misaligned RMW).
    fn rmw(
        &self,
        ctx: &mut dyn NativeContext,
        f: &dyn Fn(u64) -> Option<u64>,
    ) -> Option<(u64, bool)> {
        use cratonvm_types::ArrayElementType as E;
        if self.scale > self.width {
            let idx = self.byte_off / self.scale;
            let shift = (self.byte_off % self.scale) * 8;
            let mut attempts = 0usize;
            loop {
                let old = ctx.get_array_element(self.obj, idx);
                let bits = self.elem_bits(&old)?;
                let cur = (bits >> shift) & self.mask();
                let Some(upd) = f(cur) else {
                    return Some((cur, false));
                };
                let new_bits = (bits & !(self.mask() << shift)) | ((upd & self.mask()) << shift);
                let new = match self.elem {
                    E::Long => Value::Long(new_bits as i64),
                    E::Int => Value::Int(new_bits as u32 as i32),
                    E::Short => Value::Int(new_bits as u16 as i16 as i32),
                    E::Char => Value::Int(new_bits as u16 as i32),
                    _ => return None,
                };
                if ctx.compare_and_swap_field(self.obj, idx, old, new) {
                    return Some((cur, true));
                }
                attempts = attempts.wrapping_add(1);
                if attempts % 1024 == 0 {
                    std::thread::yield_now();
                }
            }
        }
        // nio21 wave 6 L3: a short over two byte elements is one 2-byte
        // hardware atomic on the payload when the VM can serve it (aligned:
        // a plain `AtomicU16`; misaligned: x86-64 `LOCK CMPXCHG`, as HotSpot),
        // so it is atomic against a concurrent byte store too. The lock below
        // is the fallback (no flat payload, another architecture, a split
        // lock, `CRATONVM_UNSAFE_MISALIGNED_HW_CAS=0`).
        if let Some(r) = ctx.primitive_array_atomic_update(self.obj, self.byte_off, self.width, f) {
            return Some(r);
        }
        static STRADDLE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = STRADDLE.lock().unwrap_or_else(|e| e.into_inner());
        let cur = self.load(ctx)?;
        let Some(upd) = f(cur) else {
            return Some((cur, false));
        };
        for i in 0..self.width {
            let b = (upd >> (8 * i)) & 0xff;
            let v = if self.elem == E::Boolean {
                b as i32
            } else {
                b as u8 as i8 as i32
            };
            ctx.set_array_element(self.obj, self.byte_off + i, Value::Int(v));
        }
        Some((cur, true))
    }

    /// The witnessed lane as the `B`/`S` the JDK method returns (sign-extended).
    fn signed(&self, v: u64) -> Value {
        Value::Int(if self.width == 1 {
            v as u8 as i8 as i32
        } else {
            v as u16 as i16 as i32
        })
    }
}

/// The byte/short atomics of `Unsafe` (`Cas`, `Cae`, `Add`, `Set`) over an
/// array byte lane; see [`SubwordLane`]. `None`: not that shape, the caller
/// keeps the width-matched path. `args` as in [`unsafe_arena_atomic`].
fn unsafe_subword_array_atomic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: ArenaAtomic,
    width: usize,
) -> Option<Value> {
    let obj = unsafe_obj(args, 1)?;
    let lane = SubwordLane::locate(ctx, obj, unsafe_offset(args, 2), width)?;
    let mask = lane.mask();
    let arg = |i: usize| -> u64 {
        match args.get(i) {
            Some(Value::Int(v)) => *v as i64 as u64 & mask,
            Some(Value::Long(v)) => *v as u64 & mask,
            _ => 0,
        }
    };
    let (a, b) = (arg(3), arg(4));
    let (cur, ok) = match op {
        ArenaAtomic::Cas | ArenaAtomic::Cae => lane.rmw(ctx, &|cur| (cur == a).then_some(b))?,
        ArenaAtomic::Add => lane.rmw(ctx, &|cur| Some(cur.wrapping_add(a) & mask))?,
        ArenaAtomic::Set => lane.rmw(ctx, &|_| Some(a))?,
    };
    Some(if op == ArenaAtomic::Cas {
        Value::Int(i32::from(ok))
    } else {
        lane.signed(cur)
    })
}

/// A plain or volatile `put{Byte,Short,Char,Boolean}` over an array byte lane:
/// `bytes` is the value in memory order (1 or 2 bytes). `true` when it wrote.
fn unsafe_subword_array_store(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    bytes: &[u8],
) -> bool {
    let Some(lane) = SubwordLane::locate(ctx, obj, offset, bytes.len()) else {
        return false;
    };
    let v = bytes
        .iter()
        .enumerate()
        .fold(0u64, |acc, (i, b)| acc | (u64::from(*b) << (8 * i)));
    lane.rmw(ctx, &|_| Some(v)).is_some()
}

/// Whether a sub-word access of `width` bytes at `(args[1], args[2])` is raw
/// (arena) memory or an array byte lane, i.e. a shape the width-matched
/// volatile natives (which index an element or read a side store) get wrong.
fn unsafe_subword_is_real_bytes(ctx: &dyn NativeContext, args: &[Value], width: usize) -> bool {
    match unsafe_obj(args, 1) {
        None => unsafe_arena_addr_is_tagged(unsafe_raw_addr(args, 2)),
        Some(o) => SubwordLane::locate(ctx, o, unsafe_offset(args, 2), width).is_some(),
    }
}

/// JIT round 20 wave 12 (lane qUnsafe, fuzz `R20FuzzEUnsafeArr1`): is the
/// base a `boolean[]`? Its 1-byte volatile accesses take the raw byte
/// accessors (`native_unsafe_{get,put}_byte_mb`), not the element natives,
/// whose boolean element store narrows to `v & 1` and whose load
/// zero-extends; HotSpot moves the raw byte.
fn unsafe_is_boolean_array(ctx: &dyn NativeContext, args: &[Value]) -> bool {
    unsafe_obj(args, 1).is_some_and(|o| {
        ctx.heap_kind_of(o) == cratonvm_types::ObjectKind::Array
            && matches!(
                ctx.heap_element_type_of(o),
                cratonvm_types::ArrayElementType::Boolean
            )
    })
}

macro_rules! unsafe_subword_volatile {
    ($get:ident, $put:ident, $mbget:ident, $mbput:ident, $width:expr, $access:expr) => {
        /// A volatile sub-word load: raw memory and array byte lanes by the
        /// plain accessor (which reads the REAL bytes), everything else by the
        /// width-matched volatile native.
        pub(crate) fn $get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            if unsafe_subword_is_real_bytes(ctx, args, $width)
                || ($width == 1 && unsafe_is_boolean_array(ctx, args))
            {
                return $mbget(ctx, args);
            }
            // Interpreter round i4 wave 95, lane L3: a FIELD of another kind,
            // or an offset inside a field, is the field view's, as for the
            // plain getters; the int-width volatile native answered the whole
            // slot (`getShortVolatile` of a `char` of 0xFFFF: 65535, HotSpot
            // -1). A same-kind access at a field's own offset keeps the
            // volatile native.
            if let Some(v) = unsafe_field_view_get(&*ctx, args, $access, false) {
                return Ok(Some(v));
            }
            native_unsafe_get_int_volatile(ctx, args)
        }

        /// The store half; see the getter.
        pub(crate) fn $put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            if unsafe_subword_is_real_bytes(ctx, args, $width)
                || ($width == 1 && unsafe_is_boolean_array(ctx, args))
            {
                return $mbput(ctx, args);
            }
            // JIT round 20 wave 8c: under byte field offsets, a store of
            // `$width` bytes inside / narrower than an instance field
            // replaces only those bytes (the int-width native would write
            // four).
            if ctx.unsafe_byte_field_offsets() {
                if let (Some(obj), Some(Value::Int(v))) = (unsafe_obj(args, 1), args.get(3)) {
                    // Cast: the value's low bytes in memory order.
                    let bytes = (*v as u32).to_le_bytes();
                    if unsafe_narrow_field_store(ctx, obj, unsafe_offset(args, 2), &bytes[..$width])
                    {
                        return Ok(None);
                    }
                }
            }
            native_unsafe_put_int_volatile(ctx, args)
        }
    };
}

unsafe_subword_volatile!(
    native_unsafe_get_byte_volatile,
    native_unsafe_put_byte_volatile,
    native_unsafe_get_byte_mb,
    native_unsafe_put_byte_mb,
    1,
    b'B'
);
unsafe_subword_volatile!(
    native_unsafe_get_short_volatile,
    native_unsafe_put_short_volatile,
    native_unsafe_get_short_mb,
    native_unsafe_put_short_mb,
    2,
    b'S'
);
unsafe_subword_volatile!(
    native_unsafe_get_char_volatile,
    native_unsafe_put_char_volatile,
    native_unsafe_get_char_mb,
    native_unsafe_put_char_mb,
    2,
    b'C'
);

/// `Unsafe.compareAndSetByte`: [`unsafe_arena_atomic`] over an arena handle,
/// [`unsafe_subword_array_atomic`] over an array lane, else the int-width
/// native (a heap field's slot holds the byte widened to `Int`).
pub(crate) fn native_unsafe_cas_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Cas, 1) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cas, 1) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Cas, 1) {
        return Ok(Some(v));
    }
    native_unsafe_cas_int_element(ctx, args)
}

// ---------------------------------------------------------------------------
// The `Unsafe` FIELD VIEW (interpreter round i4 wave 87, lane L3).
//
// HotSpot field offsets are byte offsets, so an access whose width or type
// differs from the field's reads or writes only the bytes it names:
// `putInt(o, offsetOf(long l), v)` replaces the low half of `l` (little-endian)
// and keeps the high half; `getShort(o, offsetOf(char c))` is the 16 bits
// read as a signed short; `getInt(o, offsetOf(float f))` is its bits. CratonVM
// field offsets are slot indices: the access replaced or narrowed the whole
// slot (`l` lost its high half, `getShort` of a char answered 65535). The view
// decodes the field once by offset (`instance_field_descriptor_byte`, the
// cached descriptor the descriptor-aware `get_field` uses) and moves bytes
// only when the access kind differs from the field's; a same-kind access
// (and every array, static, raw-memory and synthetic-offset access) takes the
// unchanged path.
//
// What the slot model cannot give, chosen and recorded:
// * an access WIDER than the field (`getLong` at an `int` field's offset):
//   HotSpot reads the neighbouring bytes, a layout this VM does not have; the
//   bytes past the field read as 0, and a wider store keeps only the field's
//   own bytes (HotSpot would also overwrite the neighbour);
// * `offset + k` inside a field names ANOTHER slot here (a slot index, not a
//   byte); it is not modelled;
// * a primitive store into a REFERENCE field is IGNORED (HotSpot writes bits
//   into an oop and corrupts the heap; ignoring keeps the slot a reference the
//   collector can trace); a primitive read of one keeps its old answer.
// ---------------------------------------------------------------------------

/// Byte width of a primitive descriptor / access kind.
fn field_view_width(kind: u8) -> Option<usize> {
    match kind {
        b'B' | b'Z' => Some(1),
        b'S' | b'C' => Some(2),
        b'I' | b'F' => Some(4),
        b'J' | b'D' => Some(8),
        _ => None,
    }
}

/// `(slot, byte within the slot)` of the INSTANCE field `(args[1], args[2])`
/// names: the offset decoded per this VM's offset scheme
/// ([`unsafe_field_slot`]: slot indices, or `16 + slot * 8 + k` under
/// `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`). `None` for a null base, an array, a
/// synthetic (minted / static) offset, or a slot past the object's fields.
///
/// Interpreter round i4 wave 95, lane L3 (page
/// `interpreter-i94-orch-dev-jit-round-20-regresses-four-interpreter-probes-FIXED-20261128.md`):
/// the view and the `char` short-atomics compared the RAW offset with the
/// field count. Once byte field offsets went default ON (JIT round 20,
/// `b0dc8fb375`) every instance offset (`16 + 8 * slot`) failed that test, the
/// view declined, and a narrow read fell to `native_unsafe_get_int`, whose
/// whole-slot answer no return narrows: `getShort` of a `long` answered its
/// low int, `getChar` of a `short` -2, `getByte` of an `int` the int, and
/// `compareAndSetChar` of a `char` >= 0x8000 compared 65535 with -1.
fn unsafe_instance_field_target(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Option<(ObjectRef, usize, usize)> {
    let obj = unsafe_obj(args, 1)?;
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    let offset = unsafe_offset(args, 2);
    if offset == BUFFER_ADDRESS_SENTINEL || unsafe_synthetic_field_offset(ctx, obj, offset) {
        return None;
    }
    let (slot, k) = unsafe_field_slot(ctx, obj, offset);
    if slot >= ctx.object_num_fields(obj) {
        return None;
    }
    Some((obj, slot, k))
}

/// The field at `(args[1], args[2])` when `access` differs in kind from it:
/// `(obj, slot, field kind)`. `None` sends the access down the unchanged path.
/// Only the field's own offset (`k == 0`); an offset inside a field is
/// [`unsafe_field_view_get`]'s sub-field arm or the stores' lane path.
fn field_view_target(
    ctx: &dyn NativeContext,
    args: &[Value],
    access: u8,
) -> Option<(ObjectRef, usize, u8)> {
    let (obj, slot, k) = unsafe_instance_field_target(ctx, args)?;
    if k != 0 {
        return None;
    }
    let field = ctx.instance_field_descriptor_byte(obj, slot)?;
    let same = field == access || (matches!(field, b'B' | b'Z') && matches!(access, b'B' | b'Z'));
    (!same).then_some((obj, slot, field))
}

/// The field's current bits, zero-extended from its width.
fn field_view_bits(value: Value, field: u8) -> u64 {
    let bits = match value {
        Value::Int(i) => i as u32 as u64,
        Value::Long(l) => l as u64,
        Value::Float(f) => f.to_bits() as u64,
        Value::Double(d) => d.to_bits(),
        _ => 0,
    };
    match field_view_width(field) {
        Some(8) | None => bits,
        Some(w) => bits & ((1u64 << (w * 8)) - 1),
    }
}

/// A value of kind `kind` from its low bits.
fn field_view_value(bits: u64, kind: u8) -> Value {
    match kind {
        b'B' => Value::Int(bits as u8 as i8 as i32),
        b'Z' => Value::Int(bits as u8 as i32),
        b'S' => Value::Int(bits as u16 as i16 as i32),
        b'C' => Value::Int(bits as u16 as i32),
        b'I' => Value::Int(bits as u32 as i32),
        b'F' => Value::Float(f32::from_bits(bits as u32)),
        b'J' => Value::Long(bits as i64),
        _ => Value::Double(f64::from_bits(bits)),
    }
}

/// Reverse the low `width` bytes (`Unsafe.convEndian` of a big-endian
/// `*Unaligned` access).
fn field_view_swap(bits: u64, width: usize) -> u64 {
    let le = bits.to_le_bytes();
    let mut out = [0u8; 8];
    for i in 0..width {
        out[i] = le[width - 1 - i];
    }
    u64::from_le_bytes(out)
}

/// A mixed-kind `get` of `access` (`B Z S C I F J D`) at a field's offset;
/// `big_endian` for an `*Unaligned(.., true)` read. `None`: not a field view.
pub(crate) fn unsafe_field_view_get(
    ctx: &dyn NativeContext,
    args: &[Value],
    access: u8,
    big_endian: bool,
) -> Option<Value> {
    // Wave 95 (lane L3): an offset INSIDE a field (byte field offsets, `k !=
    // 0`) reads `width` of its bytes from byte `k` (clamped to the field,
    // zero past it or for a reference field, as `native_unsafe_get_int`'s
    // sub-field arm), typed as the ACCESS: the int-width fallback answered
    // four bytes un-narrowed for `getShort` / `getChar` / `getByte` there too.
    let (obj, slot, k) = unsafe_instance_field_target(ctx, args)?;
    if k != 0 {
        let width = field_view_width(access)?;
        let mut bits = unsafe_sub_field_load(ctx, obj, slot, k, width);
        if big_endian {
            bits = field_view_swap(bits, width);
        }
        return Some(match access {
            b'Z' => Value::Int(i32::from(bits as u8 != 0)),
            _ => field_view_value(bits, access),
        });
    }
    let field = ctx.instance_field_descriptor_byte(obj, slot)?;
    let same = field == access || (matches!(field, b'B' | b'Z') && matches!(access, b'B' | b'Z'));
    if same {
        return None;
    }
    // A reference field (`L` / `[`) has no width: its old answer stands.
    field_view_width(field)?;
    let width = field_view_width(access)?;
    let mut bits = field_view_bits(ctx.get_field(obj, slot), field);
    if width < 8 {
        bits &= (1u64 << (width * 8)) - 1;
    }
    if big_endian {
        bits = field_view_swap(bits, width);
    }
    Some(match access {
        // `normalize_for_read(jboolean)`: any non-zero byte is `true`.
        b'Z' => Value::Int(i32::from(bits as u8 != 0)),
        _ => field_view_value(bits, access),
    })
}

/// The store half of [`unsafe_field_view_get`]: the value is `args[3]`, the
/// `*Unaligned` order flag `args[4]`. `true` when handled here (a primitive
/// store into a reference field is handled by ignoring it; see the banner).
pub(crate) fn unsafe_field_view_put(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    access: u8,
) -> bool {
    let Some((obj, slot, field)) = field_view_target(&*ctx, args, access) else {
        return false;
    };
    let Some(width) = field_view_width(access) else {
        return false;
    };
    let Some(field_width) = field_view_width(field) else {
        // `L` / `[`: a reference slot. Ignored, never overwritten with bits.
        return matches!(field, b'L' | b'[');
    };
    let mut val = match args.get(3) {
        Some(Value::Int(i)) => *i as u32 as u64,
        Some(Value::Long(l)) => *l as u64,
        Some(Value::Float(f)) => f.to_bits() as u64,
        Some(Value::Double(d)) => d.to_bits(),
        _ => 0,
    };
    if access == b'Z' {
        // `normalize_for_write(jboolean)`.
        val &= 1;
    }
    if width < 8 {
        val &= (1u64 << (width * 8)) - 1;
    }
    if matches!(args.get(4), Some(Value::Int(z)) if *z != 0) {
        val = field_view_swap(val, width);
    }
    let new_bits = if width < field_width {
        let mask = (1u64 << (width * 8)) - 1;
        (field_view_bits(ctx.get_field(obj, slot), field) & !mask) | val
    } else {
        val
    };
    ctx.set_field(obj, slot, field_view_value(new_bits, field));
    true
}

/// The descriptor's first byte of the INSTANCE field at heap slot `slot` of
/// `obj` (its class or a superclass), or `None`.
fn instance_field_descriptor_at(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    slot: usize,
) -> Option<u8> {
    let mut cid = Some(ctx.class_id_of_object(obj));
    let mut hops = 0;
    while let Some(c) = cid {
        if let Some(f) = ctx
            .declared_fields(c)
            .into_iter()
            .find(|f| !f.is_static && f.slot_index == slot)
        {
            return f.descriptor.as_bytes().first().copied();
        }
        hops += 1;
        if hops > 64 {
            return None;
        }
        cid = ctx.superclass_of(c);
    }
    None
}

/// The short-width atomics on a `char` INSTANCE field (interpreter round i4
/// wave 86, lane L3). The JDK's `compareAndSetChar` / `compareAndExchangeChar`
/// / `getAndAddChar` / `getAndSetChar` reach the `Short` natives with the char
/// narrowed to a SIGNED short (`c2s`), while a `char` field's slot holds the
/// value ZERO-extended: for a char of `0x8000` or more the int-width element
/// CAS compared `65535` with `-1`, missed, and `compareAndSetChar` (which is
/// `compareAndExchangeChar(..) == expected`, both sides `s2c`'d) still
/// answered `true` with nothing written; a successful one stored the
/// sign-extended short, which `getfield` then read back negative. HotSpot
/// compares and stores the 16 bits. This compares the low 16 bits, stores the
/// new value zero-extended, and answers the witnessed value as a short.
/// `None` for anything that is not a heap object's `C` field.
fn char_field_short_atomic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: ArenaAtomic,
) -> Option<MethodCallResult> {
    // Wave 95 (lane L3): the slot decoded from the offset (byte field offsets
    // are `16 + 8 * slot`; the raw offset failed the field-count test and the
    // int-width CAS compared 65535 with -1 again). An offset inside the field
    // is the lane atomics' (`unsafe_field_lane_atomic`).
    let (obj, offset, k) = unsafe_instance_field_target(&*ctx, args)?;
    if k != 0 || instance_field_descriptor_at(&*ctx, obj, offset) != Some(b'C') {
        return None;
    }
    let arg16 = |i: usize| match args.get(i) {
        Some(Value::Int(v)) => (*v as u32 & 0xFFFF) as i32,
        _ => 0,
    };
    let expected = arg16(3);
    // `getAndAddShort(o, off, delta)` / `getAndSetShort(o, off, v)` carry the
    // operand in arg 3; the CAS pair in arg 4.
    let operand = match op {
        ArenaAtomic::Add | ArenaAtomic::Set => arg16(3),
        _ => arg16(4),
    };
    // Bounded: a slot whose stored shape never equals what `get_field`
    // answers would otherwise spin; the int-width path is the fallback.
    for _ in 0..100_000 {
        let raw = match ctx.get_field(obj, offset) {
            Value::Int(v) => v,
            _ => 0,
        };
        let cur = (raw as u32 & 0xFFFF) as i32;
        let as_short = Value::Int(cur as u16 as i16 as i32);
        let new = match op {
            ArenaAtomic::Cas | ArenaAtomic::Cae => {
                if cur != expected {
                    return Some(Ok(Some(match op {
                        ArenaAtomic::Cas => Value::Int(0),
                        _ => as_short,
                    })));
                }
                operand
            }
            ArenaAtomic::Add => (cur + operand) & 0xFFFF,
            ArenaAtomic::Set => operand,
        };
        if ctx.compare_and_swap_field(obj, offset, Value::Int(raw), Value::Int(new)) {
            return Some(Ok(Some(match op {
                ArenaAtomic::Cas => Value::Int(1),
                _ => as_short,
            })));
        }
    }
    None
}

/// `Unsafe.compareAndSetShort`; see [`native_unsafe_cas_byte`].
pub(crate) fn native_unsafe_cas_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Cas, 2) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cas, 2) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Cas, 2) {
        return Ok(Some(v));
    }
    if let Some(r) = char_field_short_atomic(ctx, args, ArenaAtomic::Cas) {
        return r;
    }
    native_unsafe_cas_int_element(ctx, args)
}

/// `Unsafe.compareAndExchangeByte`; see [`native_unsafe_cas_byte`].
pub(crate) fn native_unsafe_cae_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Cae, 1) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cae, 1) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Cae, 1) {
        return Ok(Some(v));
    }
    native_unsafe_compare_and_exchange_int_element(ctx, args)
}

/// `Unsafe.compareAndExchangeShort`; see [`native_unsafe_cas_byte`].
pub(crate) fn native_unsafe_cae_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Cae, 2) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cae, 2) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Cae, 2) {
        return Ok(Some(v));
    }
    if let Some(r) = char_field_short_atomic(ctx, args, ArenaAtomic::Cae) {
        return r;
    }
    native_unsafe_compare_and_exchange_int_element(ctx, args)
}

/// `Unsafe.getAndAddByte`; see [`native_unsafe_cas_byte`].
pub(crate) fn native_unsafe_get_and_add_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Add, 1) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Add, 1) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Add, 1) {
        return Ok(Some(v));
    }
    native_unsafe_get_and_add_int_element(ctx, args)
}

/// `Unsafe.getAndAddShort`; see [`native_unsafe_cas_byte`].
pub(crate) fn native_unsafe_get_and_add_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Add, 2) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Add, 2) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Add, 2) {
        return Ok(Some(v));
    }
    if let Some(r) = char_field_short_atomic(ctx, args, ArenaAtomic::Add) {
        return r;
    }
    native_unsafe_get_and_add_int_element(ctx, args)
}

/// `Unsafe.compareAndSetInt`. An int-width CAS over a primitive array of
/// another element type, or straddling elements (`compareAndSetFloat` over a
/// `float[]` is this call with the raw bits; a byte-array view `VarHandle`), is
/// a CAS on the raw bytes, as HotSpot's; see [`unsafe_wide_raw_target`].
pub(crate) fn native_unsafe_cas_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Int, 4)
    {
        let (ex, nv) = (
            unsafe_wide_arg_bits(args, 3, 4),
            unsafe_wide_arg_bits(args, 4, 4),
        );
        if let Some((_, ok)) = unsafe_wide_raw_cas(ctx, obj, off, 4, ex, nv) {
            return Ok(Some(Value::Int(i32::from(ok))));
        }
    }
    native_unsafe_cas_int_element(ctx, args)
}

/// [`native_unsafe_cas_int`] without the raw-bytes arm: the element-typed
/// CAS the byte/short natives fall back to (their access is not int-width).
pub(crate) fn native_unsafe_cas_int_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), expected(int), new(int)]
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cas, 4) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Cas, 4) {
        return Ok(Some(v));
    }
    let offset = unsafe_offset(args, 2);
    let expected = args.get(3).copied().unwrap_or(Value::Int(0));
    let new_val = args.get(4).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(ok) = unsafe_static_cas(ctx, offset, expected, new_val) {
                return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_int_shard(offset);
            let cur = *map.entry((ctx.vm_identity(), offset)).or_insert(0);
            let ex = if let Value::Int(e) = expected { e } else { 0 };
            let nv = if let Value::Int(n) = new_val { n } else { 0 };
            let ok = cur == ex;
            if ok {
                map.insert((ctx.vm_identity(), offset), nv);
            }
            return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // Round-9 Bug 7: pass ctx so the side store can use a GC-stable
        // identity hash instead of a raw pointer.
        let ok = synthetic_cas(ctx, obj, offset, expected, new_val);
        return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // `compare_and_swap_field` on a primitive-array element is one
        // hardware `LOCK CMPXCHG` on the element (nio21 wave 5 L3,
        // `vm_exec::hw_cas_primitive_array_element`), so it is atomic against
        // a plain element store and a compiled CAS / XADD on the same bytes;
        // only `CRATONVM_UNSAFE_ARRAY_HW_CAS=0` puts it back on the CAS lock.
        // Bounds-check the decoded index in-crate before it reaches the
        // host accessor; OOB offset => CAS fails (no heap touch).
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        let result = ctx.compare_and_swap_field(obj, idx, expected, new_val);
        return Ok(Some(Value::Int(if result { 1 } else { 0 })));
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    let result = ctx.compare_and_swap_field(obj, offset, expected, new_val);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_unsafe_cas_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), expected(long), new(long)]
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cas, 8) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Cas, 8) {
        return Ok(Some(v));
    }
    // Raw bytes for a long-width CAS over another element type (a `double[]`
    // through `compareAndSetDouble`, a byte-array view); see
    // [`native_unsafe_cas_int`].
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Long, 8)
    {
        let (ex, nv) = (
            unsafe_wide_arg_bits(args, 3, 8),
            unsafe_wide_arg_bits(args, 4, 8),
        );
        if let Some((_, ok)) = unsafe_wide_raw_cas(ctx, obj, off, 8, ex, nv) {
            return Ok(Some(Value::Int(i32::from(ok))));
        }
    }
    let offset = unsafe_offset(args, 2);
    let expected = args.get(3).copied().unwrap_or(Value::Long(0));
    let new_val = args.get(4).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some((class_id, field_index)) = unsafe_static_field_target(&*ctx, offset) {
                let cur = ctx.get_static_field(class_id, field_index);
                // Wave 127 (lane L4): see `unsafe_static_cas`.
                let new_val = unsafe_retag_bits_like(&cur, new_val);
                let ok = unsafe_cas_values_equal(cur, expected);
                if ok {
                    ctx.set_static_field(class_id, field_index, new_val);
                }
                return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_long_shard(offset);
            let cur = *map.entry((ctx.vm_identity(), offset)).or_insert(0);
            let ex = if let Value::Long(e) = expected { e } else { 0 };
            let nv = if let Value::Long(n) = new_val { n } else { 0 };
            let ok = cur == ex;
            if ok {
                map.insert((ctx.vm_identity(), offset), nv);
            }
            return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // Round-9 Bug 7: pass ctx so the side store can use a GC-stable
        // identity hash instead of a raw pointer.
        let ok = synthetic_cas(ctx, obj, offset, expected, new_val);
        return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // One hardware CAS on the element (see `native_unsafe_cas_int_element`).
        // Bounds-check the decoded index in-crate; OOB offset => CAS fails.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        let result = ctx.compare_and_swap_field(obj, idx, expected, new_val);
        return Ok(Some(Value::Int(if result { 1 } else { 0 })));
    }
    // CRATONVM_DBG_AQS_TRACE (2026-07-21): full transition ledger for the
    // j.u.c.locks synchronizer family — every state CAS with pre-value and
    // outcome, to catch the acquire/release imbalance behind the WildFly
    // CapabilityRegistry write-lock wedge.
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    let aqs_trace = aqs_trace_enabled();
    let pre_for_trace = if aqs_trace {
        Some(ctx.get_field_volatile(obj, offset))
    } else {
        None
    };
    let result = ctx.compare_and_swap_field(obj, offset, expected, new_val);
    if aqs_trace {
        let cid = ctx.class_id_of_object(obj);
        if let Some(cls) = ctx.class_name_of_id(cid) {
            if cls.starts_with("java/util/concurrent/locks/") {
                aqs_trace_line(&format!(
                    "[AQS] tid={} cas_long obj={:p} cls={} slot={} pre={:?} exp={:?} new={:?} ok={}",
                    ctx.thread_id(),
                    obj.as_ptr(),
                    cls.rsplit('/').next().unwrap_or(&cls),
                    offset,
                    pre_for_trace.unwrap_or(Value::Object(None)),
                    expected,
                    new_val,
                    result
                ));
            }
        }
    }
    // T19_H6_CAS_DIAG: log a failed CAS on a long instance field ONLY when the
    // slot did not read back as a `Long` — the Double-tagged bit pattern (or
    // `Object(None)`) this probe was written to catch. Rate-limited to 5
    // entries to avoid flooding when a livelock fires the CAS millions of times.
    //
    // It used to warn on the first five failures of ANY kind. A failed CAS
    // whose slot reads back a `Long` is the ordinary outcome of contention —
    // `ConcurrentHashMap.addCount`'s `baseCount`/`CounterCell` retry loop,
    // `AtomicLong`, `ThreadLocalRandom`'s seeder — and every one printed in the
    // strict corpus (SBR-W3 strictgate) was that shape, e.g.
    // `class=java/util/concurrent/ConcurrentHashMap slot=4 current=Long(11)
    // expected=Long(10) new=Long(11)`: another thread got there first. Those
    // WARN lines landed in five probes' merged stdout (`ChmKeySetGrowth`,
    // `ChmShadowSweep`, `JdkOnlyPlatformProbe`, `JdkOnlyCensusLoadProbe`, ...)
    // with a timestamp as their section key, which is why the corpus reported
    // ~15 "divergent sections" that were neither divergences nor stable.
    if !result {
        let current = ctx.get_field_volatile(obj, offset);
        if !matches!(current, Value::Long(_)) {
            use std::sync::atomic::{AtomicU64, Ordering};
            static CAS_DIAG_COUNT: AtomicU64 = AtomicU64::new(0);
            let n = CAS_DIAG_COUNT.fetch_add(1, Ordering::Relaxed);
            if n < 5 {
                let cid = ctx.class_id_of_object(obj);
                let cls = ctx
                    .class_name_of_id(cid)
                    .unwrap_or_else(|| "<?>".to_string());
                tracing::warn!(
                    target: "cas_diag",
                    "T19_H6_CAS_DIAG cas_long FAIL #{} class={} slot={} current={:?} expected={:?} new={:?}",
                    n, cls, offset, current, expected, new_val
                );
            }
        }
    }
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// WP4.1 closer — recover an Object reference from a tag-lost `Value::Double`.
///
/// Under heavy AQS contention the invoke-argument pop boundary surfaces an
/// untagged 64-bit raw-pointer stack slot as `Value::Double(...)` because
/// `CompactValue::to_value()` decodes any non-NaN-tagged u64 as `Double`.
/// Native methods whose Java signature declares a reference parameter must
/// recognize this and reconstruct the original `ObjectRef`; otherwise the
/// CAS or store is performed against a denormal-shaped double, corrupting
/// the field's value tag and crashing later reads with "expected object
/// reference, got double(...)".
///
/// C20 — heap-membership filter on Java-controlled `ObjectRef::from_raw`
/// reconstruction. Mirrors the `vm::interpreter::pop_object_ref_ctx_with`
/// C7 fix: alignment + 48-bit-range alone are NOT sufficient to prove a
/// Double slot holds a smuggled `ObjectRef`. An honest `f64` whose
/// `to_bits()` value satisfies the predicate (e.g. denormals like
/// `f64::from_bits(0x800)`) would be coerced into a wild `ObjectRef`
/// and dereferenced by the next heap access. The interpreter fix uses
/// `heap.is_heap_addr(bits) -> Option<ObjectRef>` as the gating
/// primitive, but `NativeContext` (the only handle into the runtime
/// from native-builtins) does not currently expose that probe and the
/// function's call sites span both `lib.rs` and `unsafe_natives.rs` so
/// the signature is fixed.
///
/// Conservative resolution until the heap probe is plumbed through:
/// disable the smuggled-pointer reconstruction and return `Object(None)`
/// for the Double-bits arm. This matches the existing miss-idiom (line
/// just below for misaligned bits, and for `Int(0)`/`Long(0)`), and
/// matches `recover_class_mirror_from_slot`'s heap-membership-gated
/// behavior in `lang_invoke.rs`. The AQS smuggle path will instead hit
/// the "expected object reference, got double(...)" error rather than
/// silently feeding a wild pointer to downstream heap accesses — a
/// loud failure is strictly safer than the soundness footgun.
pub(crate) fn recover_object_arg(value: Value) -> Value {
    match value {
        Value::Object(_) => value,
        Value::Double(d) => {
            let bits = d.to_bits();
            if bits == 0 {
                Value::Object(None)
            } else {
                // C20: previously this fell through to
                // `ObjectRef::from_raw(bits as *mut u8)` after an
                // alignment+48-bit-range check. Without an
                // `is_heap_addr`-style heap-membership probe on
                // `NativeContext`, we cannot distinguish a legitimate
                // smuggled pointer from an honest scalar that happens to
                // satisfy the predicate. Refuse to fabricate the
                // `ObjectRef`; let the caller observe an explicit
                // null/error rather than silently dereferencing arbitrary
                // Java-controlled bits.
                Value::Object(None)
            }
        }
        Value::Int(0) | Value::Long(0) => Value::Object(None),
        _ => value,
    }
}

pub(crate) fn native_unsafe_cas_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), expected(Object), new(Object)]
    let offset = unsafe_offset(args, 2);
    // WP4.1 closer — recover Object from a tag-lost Double pattern at the
    // invoke-arg boundary; see `recover_object_arg` for rationale.
    let expected = recover_object_arg(args.get(3).copied().unwrap_or(Value::Object(None)));
    let new_val = recover_object_arg(args.get(4).copied().unwrap_or(Value::Object(None)));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(ok) = unsafe_static_cas(ctx, offset, expected, new_val) {
                return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let mut map = lock_static_obj_shard(offset);
            let cur = *map.entry(key).or_insert(None);
            let ex = if let Value::Object(e) = expected {
                e
            } else {
                None
            };
            let nv = if let Value::Object(n) = new_val {
                n
            } else {
                None
            };
            let ok = cur == ex;
            if ok {
                map.insert(key, nv);
            }
            return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
        }
    };
    // Synthetic-offset CAS — routed via per-object side store so lazy-init
    // guards on JDK-internal fields (Class.reflectionData, ServicesCatalog,
    // AbstractClassLoaderValue) don't livelock when the field's real heap
    // slot is unknown to our layout.  See `synthetic_offset_for` above.
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // Round-9 Bug 7: pass ctx so the side store can use a GC-stable
        // identity hash instead of a raw pointer.
        let ok = synthetic_cas(ctx, obj, offset, expected, new_val);
        return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
    }
    // ConcurrentHashMap.casTabAt passes byte offsets into an Object[] array,
    // not field slot indices. The shared-VM `compare_and_swap_field` path
    // dispatches on `kind_of(obj) == Array` internally AND grabs the per-
    // object CAS lock — the prior lock-free load+compare+store here did
    // NOT. Observable on `ChmStress` where 64 worker threads racing through
    // `casTabAt` would interleave inside this native and duplicate / drop
    // bin-list head installs. Pre-converting the byte offset to an element
    // index keeps the contract: `compare_and_swap_field` reuses the second
    // arg directly when `is_array` is true.
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // Bounds-check the decoded index in-crate; OOB offset => CAS fails.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        let result = ctx.compare_and_swap_field(obj, idx, expected, new_val);
        return Ok(Some(Value::Int(if result { 1 } else { 0 })));
    }
    // Wave 8c: the slot; an offset inside a field names no reference, so
    // the CAS fails.
    let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
    if k != 0 {
        return Ok(Some(Value::Int(0)));
    }
    let aqs_trace = aqs_trace_enabled();
    let pre_for_trace = if aqs_trace {
        Some(ctx.get_field_volatile(obj, offset))
    } else {
        None
    };
    let result = ctx.compare_and_swap_field(obj, offset, expected, new_val);
    if aqs_trace {
        let cid = ctx.class_id_of_object(obj);
        if let Some(cls) = ctx.class_name_of_id(cid) {
            if cls.starts_with("java/util/concurrent/locks/") {
                aqs_trace_line(&format!(
                    "[AQS] tid={} cas_obj obj={:p} cls={} slot={} pre={:?} exp={:?} new={:?} ok={}",
                    ctx.thread_id(),
                    obj.as_ptr(),
                    cls.rsplit('/').next().unwrap_or(&cls),
                    offset,
                    pre_for_trace.unwrap_or(Value::Object(None)),
                    expected,
                    new_val,
                    result
                ));
            }
        }
    }
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_unsafe_get_int_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(v) = unsafe_static_get(ctx, offset) {
                return Ok(Some(match v {
                    Value::Int(i) => Value::Int(i),
                    Value::Long(l) => Value::Int(l as i32),
                    // Interpreter round i6 wave 127, lane L4: a `float` static
                    // passes through, as the heap branch passes a `float`
                    // field: the `getFloatVolatile` registrations share this
                    // native, and `getIntVolatile`'s wrapper
                    // (`native_unsafe_get_int_volatile_arena`) answers its
                    // bits. It read as 0, so the JDK's `getAndSetFloat` loop
                    // compared 0 against the slot.
                    Value::Float(f) => Value::Float(f),
                    _ => Value::Int(0),
                }));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_int_shard(offset);
            return Ok(Some(Value::Int(map.get(&key).copied().unwrap_or(0))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(match synthetic_get(ctx, obj, offset) {
            Value::Int(v) => Value::Int(v),
            // Wave 127 (lane L4): a `float` static through its mirror base;
            // see the null-base arm above.
            Value::Float(f) => Value::Float(f),
            _ => Value::Int(0),
        }));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => Ok(Some(ctx.get_array_element(obj, idx))),
            None => Ok(Some(Value::Int(0))),
        }
    } else {
        // Wave 8c: an offset INSIDE a field reads its bytes.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the low 32 bits of the zero-extended bytes.
            (slot, k) if k != 0 => Ok(Some(Value::Int(
                unsafe_sub_field_load(&*ctx, obj, slot, k, 4) as u32 as i32,
            ))),
            (slot, _) => Ok(Some(ctx.get_field_volatile(obj, slot))),
        }
    }
}

pub(crate) fn native_unsafe_put_int_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Int(i) = val { i } else { 0 };
            if unsafe_static_put(ctx, offset, Value::Int(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_int_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field replaces four of its bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 4, bits);
            return Ok(None);
        }
        ctx.set_field_volatile(obj, offset, val);
    }
    Ok(None)
}

pub(crate) fn native_unsafe_get_long_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(value) = unsafe_static_get(ctx, offset) {
                return Ok(Some(match value {
                    Value::Long(v) => Value::Long(v),
                    Value::Int(v) => Value::Long(v as i64),
                    // Wave 127 (lane L4): a `double` static passes through;
                    // see `native_unsafe_get_int_volatile`.
                    Value::Double(d) => Value::Double(d),
                    _ => Value::Long(0),
                }));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_long_shard(offset);
            return Ok(Some(Value::Long(map.get(&key).copied().unwrap_or(0))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(match synthetic_get(ctx, obj, offset) {
            Value::Long(v) => Value::Long(v),
            Value::Int(v) => Value::Long(v as i64),
            // Wave 127 (lane L4): a `double` static through its mirror base;
            // see `native_unsafe_get_int_volatile`.
            Value::Double(d) => Value::Double(d),
            _ => Value::Long(0),
        }));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => Ok(Some(ctx.get_array_element(obj, idx))),
            None => Ok(Some(Value::Long(0))),
        }
    } else {
        // Wave 8c: an offset INSIDE a field reads its remaining bytes.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the zero-extended bytes.
            (slot, k) if k != 0 => Ok(Some(Value::Long(
                unsafe_sub_field_load(&*ctx, obj, slot, k, 8) as i64,
            ))),
            (slot, _) => Ok(Some(ctx.get_field_volatile(obj, slot))),
        }
    }
}

pub(crate) fn native_unsafe_put_long_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Long(i) = val { i } else { 0 };
            if unsafe_static_put(ctx, offset, Value::Long(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_long_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field replaces its remaining bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 8, bits);
            return Ok(None);
        }
        ctx.set_field_volatile(obj, offset, val);
    }
    Ok(None)
}

fn native_unsafe_get_object_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(v) = unsafe_static_get(ctx, offset) {
                return Ok(Some(recover_object_arg(v)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_obj_shard(offset);
            return Ok(Some(Value::Object(map.get(&key).copied().unwrap_or(None))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(recover_object_arg(synthetic_get(ctx, obj, offset))));
    }
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => ctx.get_array_element(obj, idx),
            None => Value::Object(None),
        }
    } else {
        // Wave 8c: an offset INSIDE a field names no reference.
        match unsafe_field_slot(&*ctx, obj, offset) {
            (_, k) if k != 0 => Value::Object(None),
            (slot, _) => {
                let v = ctx.get_field_volatile(obj, slot);
                slot0_read_keep_alive(ctx, obj, slot, v)
            }
        }
    };
    // T14 + WP4.1 closer: coerce non-reference values to null OR recover an
    // ObjectRef from a tag-lost Double bit pattern.
    Ok(Some(recover_object_arg(val)))
}

fn native_unsafe_put_object_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    // WP4.1 closer — recover Object from a tag-lost Double pattern.
    let val = recover_object_arg(args.get(3).copied().unwrap_or(Value::Object(None)));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Object(o) = val { o } else { None };
            if unsafe_static_put(ctx, offset, Value::Object(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_obj_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field names no reference: no store.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            return Ok(None);
        }
        ctx.set_field_volatile(obj, offset, val);
    }
    Ok(None)
}

pub(crate) fn native_unsafe_get_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(v) = unsafe_static_get(ctx, offset) {
                return Ok(Some(recover_object_arg(v)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_obj_shard(offset);
            return Ok(Some(Value::Object(map.get(&key).copied().unwrap_or(None))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(recover_object_arg(synthetic_get(ctx, obj, offset))));
    }
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => ctx.get_array_element(obj, idx),
            None => Value::Object(None),
        }
    } else {
        // Wave 8c: an offset INSIDE a field names no reference.
        match unsafe_field_slot(&*ctx, obj, offset) {
            (_, k) if k != 0 => Value::Object(None),
            (slot, _) => {
                let v = ctx.get_field(obj, slot);
                slot0_read_keep_alive(ctx, obj, slot, v)
            }
        }
    };
    Ok(Some(recover_object_arg(val)))
}

pub(crate) fn native_unsafe_put_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    // WP4.1 closer — recover Object from a tag-lost Double pattern.
    let val = recover_object_arg(args.get(3).copied().unwrap_or(Value::Object(None)));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Object(o) = val { o } else { None };
            if unsafe_static_put(ctx, offset, Value::Object(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_obj_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field names no reference: no store.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

/// Multi-byte `Unsafe` accessors for `byte[]`-backed memory.
///
/// `Unsafe.get{Short,Char,Int,Long}` and the `*Unaligned` variants take a
/// *byte* offset into the target object.  When the target is a `byte[]`
/// (the backing store of every `HeapByteBuffer`), a single typed read must
/// assemble 2/4/8 consecutive array elements — it is NOT a single-element
/// access.  The generic `native_unsafe_get_int` returns just one element,
/// which truncates every `ByteBuffer.get{Char,Short,Int,Long}` to its first
/// byte (e.g. ICU's `ICUBinary.readHeader` reading the `nfc.nrm` header).
///
/// The 3-arg `*Unaligned` form carries an explicit `bigEndian` boolean at
/// args[3]; the 2-arg form (and the plain aligned `getChar`/`getInt`/…)
/// uses the platform's native byte order, which on every CratonVM host
/// (x86-64 / aarch64) is little-endian.
///
/// Returns `Some(bytes)` only when `obj` is a primitive array and the
/// requested byte range fits; otherwise `None`, so the caller falls back to
/// the generic element/field path (non-array targets, off-heap, references).
///
/// Works for EVERY primitive element type — not just `byte[]`. A multi-byte
/// `Unsafe` read takes a *byte* offset and assembles `width` consecutive
/// bytes which may SPAN several elements: `getLongUnaligned(char[], …)`
/// reads 4 chars, `getLongUnaligned(int[], …)` reads 2 ints, etc. The JDK
/// lays each element out in the platform's native (little-endian) byte
/// order. The previous version handled only `byte[]`/`boolean[]` and fell
/// back to a single-ELEMENT read for `char[]`/`int[]`/`long[]`, which
/// truncated every cross-element word read to its first element — breaking
/// `jdk.internal.util.ArraysSupport.vectorizedMismatch` and therefore
/// `Arrays.equals(char[]/long[])` (e.g. ecj's `CharOperation.equals`
/// mis-comparing `"Signature"` vs `"Synthetic"`).
/// Widest `Unsafe` scalar access, and so the size of the stack buffer
/// [`unsafe_read_bytes_into`] fills. `getLongUnaligned` / `getDouble` are the
/// only 8-byte shapes; nothing in the family is wider.
const UNSAFE_MAX_ACCESS_WIDTH: usize = 8;

/// Non-allocating core of [`unsafe_read_bytes_from_array`]: fills
/// `out[..width]` and returns `true`, or returns `false` and leaves `out`
/// unspecified.
///
/// # Why a `byte[]` fast path exists here
///
/// `byte[]` is not one element type among six on this path — it is the backing
/// store of every `HeapByteBuffer`, so it is what `ByteBuffer.getInt` /
/// `getLong` / `getShort` reach, which is the hot shape in any binary-header
/// or HTTP-codec workload. The generic loop below costs `width` VIRTUAL
/// `get_array_element` calls plus, in the old shape, a `Vec` allocation, for a
/// read the heap can serve with one `memcpy`: `read_byte_array_into` is
/// overridden in the VM to `copy_nonoverlapping` from the array payload (with
/// a per-element fallback of its own for a G1 humongous array, so the fast
/// path does not have to know about that case).
///
/// The generic loop is unchanged and still covers `char[]` / `int[]` / `long[]`
/// and friends, which a multi-byte read may legitimately SPAN — the property
/// `ArraysSupport.vectorizedMismatch` depends on and the reason this function
/// is not simply a `byte[]` reader.
fn unsafe_read_bytes_into(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    width: usize,
    out: &mut [u8; UNSAFE_MAX_ACCESS_WIDTH],
) -> bool {
    use cratonvm_types::ArrayElementType as Aet;
    if width > UNSAFE_MAX_ACCESS_WIDTH {
        return false;
    }
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let elem_type = ctx.heap_element_type_of(obj);
    let elem_size: usize = match elem_type {
        Aet::Byte | Aet::Boolean => 1,
        Aet::Char | Aet::Short => 2,
        Aet::Int | Aet::Float => 4,
        Aet::Long | Aet::Double => 8,
        // Reference arrays have no byte-addressable element storage.
        Aet::Reference => return false,
    };
    // Array byte offsets always start at ABASE (16); see
    // `unsafe_array_index_from_offset` / `native_unsafe_array_base_offset`.
    const ABASE: usize = 16;
    if offset < ABASE {
        return false;
    }
    let rel = offset - ABASE; // bytes from element 0
    let len = ctx.array_length(obj); // element COUNT
    let Some(total_bytes) = len.checked_mul(elem_size) else {
        return false;
    };
    match rel.checked_add(width) {
        Some(end) if end <= total_bytes => {}
        _ => return false,
    }
    if elem_size == 1 {
        // One `memcpy`. `rel` IS the element index when elements are one byte
        // wide, and the range was bounds-checked against `total_bytes` above,
        // so a short return can only mean the heap declined the bulk route —
        // in which case fall through to the generic loop rather than answer
        // with a partly filled buffer.
        if ctx.read_byte_array_into(obj, rel, &mut out[..width]) == width {
            return true;
        }
    } else if ctx.read_primitive_array_bytes(obj, rel, &mut out[..width]) == width {
        // SBR3 ffmperf: every wider element type is a contiguous native-endian
        // payload too; one `memcpy` instead of `width` element reads.
        return true;
    }
    for (k, slot) in out.iter_mut().enumerate().take(width) {
        let byte_pos = rel + k;
        let elem_idx = byte_pos / elem_size;
        let byte_in_elem = byte_pos % elem_size;
        // Read the element's raw bit pattern, then extract the requested
        // byte in little-endian order (native order on x86-64 / aarch64).
        let elem_bits: u64 = match ctx.get_array_element(obj, elem_idx) {
            Value::Int(v) => v as u32 as u64, // byte/short/char/int (low bits used per elem_size)
            Value::Long(v) => v as u64,
            Value::Float(f) => f.to_bits() as u64,
            Value::Double(d) => d.to_bits(),
            _ => return false,
        };
        *slot = ((elem_bits >> (8 * byte_in_elem)) & 0xFF) as u8;
    }
    true
}

fn unsafe_read_bytes_from_array(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    width: usize,
) -> Option<Vec<u8>> {
    let mut buf = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
    if !unsafe_read_bytes_into(ctx, obj, offset, width, &mut buf) {
        return None;
    }
    Some(buf[..width].to_vec())
}

/// Decode the trailing `bigEndian` boolean of an `*Unaligned` call.
/// `Some(true)` = big-endian, `Some(false)` = little-endian, `None` = the
/// 2-arg form (no explicit order) → native order = little-endian.
fn unsafe_big_endian_arg(args: &[Value]) -> bool {
    match args.get(3) {
        Some(Value::Int(z)) => *z != 0,
        // 2-arg form or non-int trailing slot: native (little-endian).
        _ => false,
    }
}

pub(crate) fn scoped_memory_access_unsafe_args(args: &[Value]) -> Vec<Value> {
    let mut adapted = Vec::with_capacity(args.len().saturating_sub(1));
    adapted.push(args.first().copied().unwrap_or(Value::Object(None)));
    if args.len() > 2 {
        adapted.extend_from_slice(&args[2..]);
    }
    adapted
}

macro_rules! unsafe_multibyte_get {
    ($name:ident, $width:expr, $assemble:expr, $access:expr) => {
        pub(crate) fn $name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            // A FIELD of another kind (i4 wave 87, lane L3).
            if let Some(v) =
                unsafe_field_view_get(&*ctx, args, $access, unsafe_big_endian_arg(args))
            {
                return Ok(Some(v));
            }
            let offset = unsafe_offset(args, 2);
            if unsafe_obj(args, 1).is_none() {
                let addr = unsafe_raw_addr(args, 2);
                let mut bytes = [0u8; $width];
                if ctx.copy_from_native_memory(addr, &mut bytes) {
                    let v: i64 = $assemble(&bytes, unsafe_big_endian_arg(args));
                    return Ok(Some(Value::Int(v as i32)));
                }
            }
            if let Some(obj) = unsafe_obj(args, 1) {
                // Stack buffer, not a `Vec`: this is the per-accessor path
                // under every `HeapByteBuffer.getShort`/`getInt`, so the
                // allocation was one malloc per scalar read.
                let mut buf = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
                if unsafe_read_bytes_into(ctx, obj, offset, $width, &mut buf) {
                    let big_endian = unsafe_big_endian_arg(args);
                    let v: i64 = $assemble(&buf[..$width], big_endian);
                    return Ok(Some(Value::Int(v as i32)));
                }
            }
            // Not a primitive-array/direct-memory target: generic element/field access.
            native_unsafe_get_int(ctx, args)
        }
    };
}

// `getShort` → signed 16-bit; `getChar` → unsigned 16-bit; `getInt` → 32-bit.
unsafe_multibyte_get!(native_unsafe_get_short_mb, 2, asm_i16, b'S');

unsafe_multibyte_get!(native_unsafe_get_char_mb, 2, asm_u16, b'C');

unsafe_multibyte_get!(native_unsafe_get_int_mb, 4, asm_i32, b'I');

/// 64-bit `Unsafe` get for `byte[]`-backed memory (`getLong`/`getLongUnaligned`).
pub(crate) fn native_unsafe_get_long_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if let Some(v) = unsafe_field_view_get(&*ctx, args, b'J', unsafe_big_endian_arg(args)) {
        return Ok(Some(v));
    }
    let offset = unsafe_offset(args, 2);
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let mut b = [0u8; 8];
        if ctx.copy_from_native_memory(addr, &mut b) {
            let big_endian = unsafe_big_endian_arg(args);
            let v = if big_endian {
                i64::from_be_bytes(b)
            } else {
                i64::from_le_bytes(b)
            };
            return Ok(Some(Value::Long(v)));
        }
    }
    if let Some(obj) = unsafe_obj(args, 1) {
        // As the multi-byte `get` macro: a stack buffer, because this is the
        // body of `HeapByteBuffer.getLong`.
        let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
        if unsafe_read_bytes_into(ctx, obj, offset, 8, &mut b) {
            let big_endian = unsafe_big_endian_arg(args);
            let v = if big_endian {
                i64::from_be_bytes(b)
            } else {
                i64::from_le_bytes(b)
            };
            return Ok(Some(Value::Long(v)));
        }
    }
    native_unsafe_get_long(ctx, args)
}

/// Multi-byte `Unsafe` put for `byte[]`-backed memory. Scatters a typed
/// value into `width` consecutive array elements honoring endianness.
/// Returns `true` if the write was handled here (byte-array target).
fn unsafe_write_bytes_to_byte_array(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    bytes: &[u8],
) -> bool {
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    match ctx.heap_element_type_of(obj) {
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean => {}
        _ => return false,
    }
    const ABASE: usize = 16;
    if offset < ABASE {
        return false;
    }
    let start = offset - ABASE;
    let len = ctx.array_length(obj);
    if start + bytes.len() > len {
        return false;
    }
    // One `memcpy` where the heap can give one — the store twin of
    // `unsafe_read_bytes_into`'s fast path, and the same reason: this is the
    // body of `HeapByteBuffer.putInt`/`putLong`, so the per-element loop below
    // was `width` virtual calls per scalar write.
    //
    // Falling through on `false` is safe even though the override's G1
    // humongous arm can stop partway: the loop below rewrites the WHOLE range
    // from `start`, and a byte store is idempotent, so partial progress is
    // overwritten rather than compounded. The bounds were checked above and
    // `write_byte_array_from` re-checks them itself, so neither path can run
    // off the end.
    if ctx.write_byte_array_from(obj, start, bytes) {
        return true;
    }
    for (i, &b) in bytes.iter().enumerate() {
        // Byte-array elements round-trip as sign-extended Int.
        ctx.set_array_element(obj, start + i, Value::Int(b as i8 as i32));
    }
    true
}

/// SBR3 lane ffmperf: does an `Unsafe.put*` of `width` bytes at byte `offset`
/// of a primitive array of element type `et` need the RAW (byte-spanning) store
/// rather than the typed one-element store of the access's own natural element
/// type `natural`? It does when the array's elements are another type
/// (`putLong(int[], ..)`, `putInt(short[], ..)`, `putByte(int[], ..)`: HotSpot
/// stores the bytes whatever the array holds, which is what the JDK's
/// `SegmentBulkOperations` loops over a heap segment of any carrier do) or when
/// the access straddles elements. A byte/boolean array has its own path.
fn unsafe_put_needs_raw(
    et: cratonvm_types::ArrayElementType,
    natural: cratonvm_types::ArrayElementType,
    offset: usize,
    width: usize,
) -> bool {
    use cratonvm_types::ArrayElementType as Aet;
    if matches!(et, Aet::Reference | Aet::Byte | Aet::Boolean) {
        return false;
    }
    if et != natural {
        return true;
    }
    offset >= 16 && (offset - 16) % width != 0
}

macro_rules! unsafe_multibyte_put {
    ($name:ident, $width:expr, $generic:ident, $natural:ident, $access:expr) => {
        pub(crate) fn $name(
            ctx: &mut dyn NativeContext,
            args: &[Value],
        ) -> MethodCallResult {
            // A FIELD of another kind (i4 wave 87, lane L3).
            if unsafe_field_view_put(ctx, args, $access) {
                return Ok(None);
            }
            let offset = unsafe_offset(args, 2);
            let val = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            // The bigEndian flag, when present, is args[4].
            let big_endian = matches!(args.get(4), Some(Value::Int(z)) if *z != 0);
            let raw = (val as u32) & ((1u64 << ($width * 8)) - 1) as u32;
            let mut bytes = [0u8; $width];
            if big_endian {
                for i in 0..$width {
                    bytes[$width - 1 - i] = (raw >> (i * 8)) as u8;
                }
            } else {
                for i in 0..$width {
                    bytes[i] = (raw >> (i * 8)) as u8;
                }
            }
            match unsafe_obj(args, 1) {
                None => {
                    if ctx.copy_to_native_memory(unsafe_raw_addr(args, 2), &bytes) {
                        return Ok(None);
                    }
                }
                Some(obj) => {
                    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
                        && matches!(
                            ctx.heap_element_type_of(obj),
                            cratonvm_types::ArrayElementType::Byte
                                | cratonvm_types::ArrayElementType::Boolean
                        )
                        && unsafe_write_bytes_to_byte_array(ctx, obj, offset, &bytes)
                    {
                        return Ok(None);
                    }
                    // A short inside a wider array element (`SubwordLane`):
                    // atomic against the element's other lanes.
                    if $width <= 2 && unsafe_subword_array_store(ctx, obj, offset, &bytes) {
                        return Ok(None);
                    }
                    // SBR3 ffmperf (after the lane: whatever it declines):
                    // another primitive element type, or an
                    // access straddling elements: the raw store.
                    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
                        && unsafe_put_needs_raw(
                            ctx.heap_element_type_of(obj),
                            cratonvm_types::ArrayElementType::$natural,
                            offset,
                            $width,
                        )
                        && unsafe_array_write_bytes(ctx, obj, offset, &bytes)
                    {
                        return Ok(None);
                    }
                    // JIT round 20 wave 8 (lane huntDisp): a store narrower
                    // than the instance field at this offset.
                    if unsafe_narrow_field_store(ctx, obj, offset, &bytes) {
                        return Ok(None);
                    }
                }
            }
            $generic(ctx, args)
        }
    };
}

unsafe_multibyte_put!(
    native_unsafe_put_short_mb,
    2,
    native_unsafe_put_int,
    Short,
    b'S'
);

unsafe_multibyte_put!(
    native_unsafe_put_char_mb,
    2,
    native_unsafe_put_int,
    Char,
    b'C'
);

unsafe_multibyte_put!(
    native_unsafe_put_int_mb,
    4,
    native_unsafe_put_int,
    Int,
    b'I'
);

/// 64-bit `Unsafe` put for `byte[]`-backed memory.
pub(crate) fn native_unsafe_put_long_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if unsafe_field_view_put(ctx, args, b'J') {
        return Ok(None);
    }
    let offset = unsafe_offset(args, 2);
    let val = match args.get(3) {
        Some(Value::Long(l)) => *l,
        Some(Value::Int(i)) => *i as i64,
        _ => 0,
    };
    let big_endian = matches!(args.get(4), Some(Value::Int(z)) if *z != 0);
    let bytes = if big_endian {
        (val as u64).to_be_bytes()
    } else {
        (val as u64).to_le_bytes()
    };
    if unsafe_obj(args, 1).is_none() {
        if ctx.copy_to_native_memory(unsafe_raw_addr(args, 2), &bytes) {
            return Ok(None);
        }
    }
    if let Some(obj) = unsafe_obj(args, 1) {
        if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
            && matches!(
                ctx.heap_element_type_of(obj),
                cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
            )
        {
            if unsafe_write_bytes_to_byte_array(ctx, obj, offset, &bytes) {
                return Ok(None);
            }
        }
        // SBR3 ffmperf: see `unsafe_put_needs_raw`.
        if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
            && unsafe_put_needs_raw(
                ctx.heap_element_type_of(obj),
                cratonvm_types::ArrayElementType::Long,
                offset,
                8,
            )
            && unsafe_array_write_bytes(ctx, obj, offset, &bytes)
        {
            return Ok(None);
        }
    }
    native_unsafe_put_long(ctx, args)
}

pub(crate) fn native_unsafe_get_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(v) = unsafe_static_get(ctx, offset) {
                return Ok(Some(match v {
                    Value::Int(i) => Value::Int(i),
                    Value::Long(l) => Value::Int(l as i32),
                    _ => Value::Int(0),
                }));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_int_shard(offset);
            return Ok(Some(Value::Int(map.get(&key).copied().unwrap_or(0))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(match synthetic_get(ctx, obj, offset) {
            Value::Int(v) => Value::Int(v),
            _ => Value::Int(0),
        }));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => Ok(Some(ctx.get_array_element(obj, idx))),
            None => Ok(Some(Value::Int(0))),
        }
    } else {
        // JIT round 20 wave 7 (lane huntDisp): an int-sized read AT a
        // `float` field's own offset is the field's bits, as HotSpot reads
        // its 4 bytes; the slot's `Value::Float` reached the `I` return and
        // read as 0 (`R20HuntDispUnsafeField` `pun.first`), while compiled
        // code answered the bits.
        // Wave 8c: an offset INSIDE a field (byte field offsets) reads its
        // bytes; the caller's return narrows a byte / short / char read.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            // Cast: the low 32 bits of the zero-extended bytes.
            return Ok(Some(Value::Int(
                unsafe_sub_field_load(&*ctx, obj, offset, k, 4) as u32 as i32,
            )));
        }
        Ok(Some(match ctx.get_field(obj, offset) {
            Value::Float(f) => Value::Int(f.to_bits() as i32),
            // Wave 8: a narrower read of a `double` field is its low bytes.
            Value::Double(d) => Value::Int(d.to_bits() as i32),
            other => other,
        }))
    }
}

pub(crate) fn native_unsafe_put_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Int(i) = val { i } else { 0 };
            if unsafe_static_put(ctx, offset, Value::Int(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_int_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field (byte field offsets) replaces
        // four of its bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 4, bits);
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

/// `Unsafe.getByte(Object, long)` / `getBoolean` — width-correct 1-byte read.
///
/// A NULL base means an off-heap absolute address: route it through the SAME
/// arena-or-raw path the NIO socket / FileChannel I/O uses
/// (`copy_from_native_memory`) so a `DirectByteBuffer` round-trips byte-wise
/// with native reads/writes. The previous handler (`native_unsafe_get_int`)
/// stashed null-base bytes in a separate `static_int_store`, which diverged
/// from the raw/arena memory the socket layer reads — so Tomcat's http-nio
/// byte-wise request parsing saw zeros. A heap base falls back to the generic
/// field/array accessor.
pub(crate) fn native_unsafe_get_byte_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unsafe_obj(args, 1) {
        None => {
            let addr = unsafe_raw_addr(args, 2);
            let mut b = [0u8; 1];
            ctx.copy_from_native_memory(addr, &mut b);
            // getByte returns a (sign-extended) byte; getBoolean coerces 0/non-0.
            Ok(Some(Value::Int(b[0] as i8 as i32)))
        }
        Some(obj) => {
            // The low byte of a wider FIELD (i4 wave 87, lane L3).
            if let Some(v) = unsafe_field_view_get(&*ctx, args, b'B', false) {
                return Ok(Some(v));
            }
            // A byte inside a wider array element (`int[]`, `long[]`, `short[]`,
            // `char[]`, and `float[]`/`double[]`): the byte lane, not the element
            // (the `SubwordLane` shapes are a subset of this raw read; a load needs
            // no atomicity).
            // JIT round 20 wave 12 (lane qUnsafe, fuzz `R20FuzzEUnsafeArr1`):
            // a `boolean[]` byte too. The element read below zero-extends the
            // byte, where `getByte` sign-extends (HotSpot reads the raw byte).
            if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
                && !matches!(
                    ctx.heap_element_type_of(obj),
                    cratonvm_types::ArrayElementType::Reference
                )
            {
                // Raw boolean-array bytes are not baload/bastore values:
                // Unsafe.putByte can install any eight-bit pattern.
                let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
                if unsafe_read_bytes_into(ctx, obj, unsafe_offset(args, 2), 1, &mut b) {
                    return Ok(Some(Value::Int(b[0] as i8 as i32)));
                }
            }
            native_unsafe_get_int(ctx, args)
        }
    }
}

/// `Unsafe.getBoolean(Object, long)`: [`native_unsafe_get_byte_mb`]'s byte,
/// normalized as HotSpot's `normalize_for_read(jboolean)` does (`x != 0`).
/// JIT round 20 wave 7 (lane huntDisp): the raw byte reached the `Z` return,
/// whose boolean narrowing keeps bit 0 only, so a byte of 2 or -12 read as
/// `false` where HotSpot answers `true` (`R20HuntDispUnsafeField`
/// `pun.after`), in both tiers.
pub(crate) fn native_unsafe_get_boolean_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    normalize_unsafe_boolean_read(native_unsafe_get_byte_mb(ctx, args))
}

/// `Unsafe.getBooleanVolatile`: [`native_unsafe_get_byte_volatile`],
/// normalized as [`native_unsafe_get_boolean_mb`].
pub(crate) fn native_unsafe_get_boolean_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    normalize_unsafe_boolean_read(native_unsafe_get_byte_volatile(ctx, args))
}

fn normalize_unsafe_boolean_read(read: MethodCallResult) -> MethodCallResult {
    read.map(|value| {
        value.map(|v| match v {
            Value::Int(i) => Value::Int(i32::from(i != 0)),
            other => other,
        })
    })
}

/// Decode a raw off-heap ADDRESS argument with NO field-offset clamping.
///
/// The `base == null` forms of `Unsafe.{get,put}*(Object, long, …)` pass a full
/// native address in the `long` slot, not a field index. `unsafe_offset`'s
/// clamp (which exists to neutralise garbage *field* slot indices from
/// compact-value drift) zeroes any value above `MAX_REASONABLE_OFFSET` that is
/// not a tagged arena handle — which silently rewrites a **real** direct-buffer
/// pointer (e.g. a Jetty `ByteBufferPool` allocation at `0x000002_8…`) to
/// address 0. `DirectByteBuffer.put(byte)`/`get(byte)` (≤6-element transfers via
/// `ScopedMemoryAccess`) then wrote/read at address 0, so e.g. Jetty
/// HttpClient's byte-at-a-time request framing produced an all-zero request no
/// server could parse. `copy_to/from_native_memory` already validate the
/// address (arena-vs-raw routing, bounds, null reject), so passing the raw
/// value through here is safe — the clamp's field-index protection is only
/// meaningful for the `base != null` path.
pub(crate) fn unsafe_raw_addr(args: &[Value], pos: usize) -> i64 {
    match args.get(pos) {
        Some(Value::Long(a)) => *a,
        Some(Value::Int(a)) => *a as i64,
        // Compact-value drift can surface a long's bit pattern as Double/Float
        // on the invoke-arg boundary (see `unsafe_offset`); mirror its decode.
        Some(Value::Double(d)) => {
            if d.is_finite() {
                let n = *d as i64;
                if n >= 0 && (n as f64) == *d {
                    return n;
                }
            }
            d.to_bits() as i64
        }
        Some(Value::Float(f)) => {
            if f.is_finite() {
                let n = *f as i64;
                if n >= 0 && (n as f32) == *f {
                    return n;
                }
            }
            f.to_bits() as u32 as i64
        }
        _ => 0,
    }
}

/// JIT round 20, lane rev12: `Unsafe.putBoolean` / `putBooleanVolatile`
/// store `x & 1`, as HotSpot does (`MemoryAccess::normalize_for_write` in
/// `unsafe.cpp`; C2's `T_BOOLEAN` store masks the same way), so a `Z`
/// argument outside `0`/`1` -- which the verifier admits from any `int` --
/// stores its low bit, not its low byte. `None` when the value is already
/// `0`/`1` (every javac-compiled call): the byte natives then run on the
/// caller's own slice.
fn unsafe_put_boolean_args(args: &[Value]) -> Option<Vec<Value>> {
    match args.get(3) {
        Some(Value::Int(v)) if *v & !1 != 0 => {
            let mut adapted = args.to_vec();
            adapted[3] = Value::Int(*v & 1);
            Some(adapted)
        }
        _ => None,
    }
}

/// `Unsafe.putBoolean(Object, long, boolean)`: [`native_unsafe_put_byte_mb`]
/// of the normalised value ([`unsafe_put_boolean_args`]).
pub(crate) fn native_unsafe_put_boolean_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unsafe_put_boolean_args(args) {
        Some(adapted) => native_unsafe_put_byte_mb(ctx, &adapted),
        None => native_unsafe_put_byte_mb(ctx, args),
    }
}

/// `Unsafe.putBooleanVolatile(Object, long, boolean)`:
/// [`native_unsafe_put_byte_volatile`] of the normalised value
/// ([`unsafe_put_boolean_args`]).
pub(crate) fn native_unsafe_put_boolean_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unsafe_put_boolean_args(args) {
        Some(adapted) => native_unsafe_put_byte_volatile(ctx, &adapted),
        None => native_unsafe_put_byte_volatile(ctx, args),
    }
}

/// `Unsafe.putByte(Object, long, byte)` / `putBoolean` — width-correct 1-byte
/// write. See [`native_unsafe_get_byte_mb`] for the null-base routing rationale.
pub(crate) fn native_unsafe_put_byte_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unsafe_obj(args, 1) {
        None => {
            let addr = unsafe_raw_addr(args, 2);
            let v = match args.get(3) {
                Some(Value::Int(i)) => *i as u8,
                _ => 0,
            };
            ctx.copy_to_native_memory(addr, &[v]);
            Ok(None)
        }
        Some(obj) => {
            // A raw byte store must bypass boolean bastore's value & 1
            // conversion. The byte/boolean bulk accessor preserves the wire
            // byte, including its sign bit, across every collector.
            if let Some(Value::Int(v)) = args.get(3) {
                if unsafe_write_bytes_to_byte_array(ctx, obj, unsafe_offset(args, 2), &[*v as u8]) {
                    return Ok(None);
                }
            }
            // The low byte of a wider FIELD (i4 wave 87, lane L3).
            if unsafe_field_view_put(ctx, args, b'B') {
                return Ok(None);
            }
            // A byte lane of a wider element: atomic (`SubwordLane`).
            if let Some(Value::Int(v)) = args.get(3) {
                if unsafe_subword_array_store(ctx, obj, unsafe_offset(args, 2), &[*v as u8]) {
                    return Ok(None);
                }
            }
            // SBR3 ffmperf (whatever the lane declines, e.g. float[]/double[]): a byte store into a short/char/int/long/float/double
            // array lands in one BYTE of an element (`SegmentBulkOperations`'
            // tail bytes over a heap segment of any carrier), not in a whole
            // element.
            if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
                let et = ctx.heap_element_type_of(obj);
                // JIT round 20 wave 12 (lane qUnsafe, fuzz `R20FuzzEUnsafeArr1`):
                // a `boolean[]` takes the raw byte store too. The element store
                // (`set_array_element`) narrows a boolean element to `v & 1`
                // (`bastore`'s rule), so `Unsafe.putByte(boolean[], 2)` stored
                // 0 where HotSpot stores the byte 2 (and `getBoolean` then
                // answers `true`). `putBoolean` masks `x & 1` before it gets
                // here (`native_unsafe_put_boolean_mb`).
                if !matches!(
                    et,
                    cratonvm_types::ArrayElementType::Reference
                        | cratonvm_types::ArrayElementType::Byte
                ) {
                    let v = match args.get(3) {
                        Some(Value::Int(i)) => *i as u8,
                        _ => 0,
                    };
                    if unsafe_array_write_bytes(ctx, obj, unsafe_offset(args, 2), &[v]) {
                        return Ok(None);
                    }
                }
            }
            // JIT round 20 wave 8 (lane huntDisp): a byte stored AT a wider
            // instance field's offset is its low byte only.
            if let Some(Value::Int(v)) = args.get(3) {
                if unsafe_narrow_field_store(ctx, obj, unsafe_offset(args, 2), &[*v as u8]) {
                    return Ok(None);
                }
            }
            native_unsafe_put_int(ctx, args)
        }
    }
}

/// JIT round 20 wave 8 (lane huntDisp,
/// `jr20-unsafe-sub-field-offsets-alias-neighbouring-slots-FIXED-20261012.md`): an
/// `Unsafe` store of `bytes` (in memory order, as the put natives lay them
/// out) AT the offset of an instance field WIDER than the store. HotSpot
/// writes those bytes at `base + offset`, i.e. replaces the field's low
/// `bytes.len()` bytes (little-endian) and keeps the rest; the whole-slot
/// store replaced the entire value (`putByte` of `0x11` into a `short` of -2
/// gave 17, HotSpot -239). Read-modify-write of the slot's value by the
/// field's declared descriptor, re-encoded as that descriptor's `Value`.
/// `false` (nothing written) for an array, a synthetic offset, an
/// unresolvable slot, a reference field, or a store as wide as the field or
/// wider -- the caller's path then runs as before. Not atomic against a
/// concurrent writer of the same field's other bytes (a plain store; the
/// volatile and CAS natives keep their own paths).
fn unsafe_narrow_field_store(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    bytes: &[u8],
) -> bool {
    if bytes.is_empty() || ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        return false;
    }
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // Wave 11: byte `k != 0` of a static (byte field offsets), at the
        // store's own width; every other synthetic offset keeps its path.
        let Some((class_id, field_index, k)) = unsafe_static_sub_target(&*ctx, Some(obj), offset)
        else {
            return false;
        };
        let bits = bytes
            .iter()
            .rev()
            .fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        let _ = unsafe_static_lane_rmw(ctx, class_id, field_index, k, bytes.len(), &|_| Some(bits));
        return true;
    }
    // Wave 8c: under byte field offsets, a store INSIDE a field (`k != 0`)
    // replaces those bytes of it, whatever its width; it is never the
    // whole-slot store of the caller's fallback.
    let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
    if k != 0 {
        let bits = bytes
            .iter()
            .rev()
            .fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        unsafe_sub_field_store(ctx, obj, offset, k, bytes.len(), bits);
        return true;
    }
    let Some(desc) = ctx.instance_field_descriptor_byte(obj, offset) else {
        return false;
    };
    let field_width = match desc {
        b'B' | b'Z' => 1,
        b'S' | b'C' => 2,
        b'I' | b'F' => 4,
        b'J' | b'D' => 8,
        _ => return false,
    };
    if bytes.len() >= field_width {
        return false;
    }
    let bits: u64 = match ctx.get_field(obj, offset) {
        Value::Int(i) => i as u32 as u64,
        Value::Long(l) => l as u64,
        Value::Float(f) => f.to_bits() as u64,
        Value::Double(d) => d.to_bits(),
        _ => return false,
    };
    let mut le = bits.to_le_bytes();
    le[..bytes.len()].copy_from_slice(bytes);
    let merged = u64::from_le_bytes(le);
    // Casts: each field's value is its low `field_width` bytes, re-extended
    // as the descriptor's Java type.
    let value = match desc {
        b'S' => Value::Int(merged as u16 as i16 as i32),
        b'C' => Value::Int(merged as u16 as i32),
        b'I' => Value::Int(merged as u32 as i32),
        b'F' => Value::Float(f32::from_bits(merged as u32)),
        b'J' => Value::Long(merged as i64),
        b'D' => Value::Double(f64::from_bits(merged)),
        _ => return false,
    };
    ctx.set_field(obj, offset, value);
    true
}

pub(crate) fn native_unsafe_get_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    // C32: Buffer.address sentinel. `objectFieldOffset(Buffer.address)` returns
    // this sentinel; `getLong(buffer, sentinel)` must return that buffer's REAL
    // native address — Netty's `PlatformDependent0.directBufferAddress()` uses
    // exactly this path to compute `memoryAddress()` for *every* direct buffer,
    // not just the `<clinit>` availability probe. Returning a constant `1` made
    // every direct buffer's address `0x1`, so the first write SIGSEGV'd
    // (crash-03: rsocket/Netty PooledByteBuf). Read the receiver's `address`
    // field; fall back to `1` (non-zero, so the init probe still passes) only
    // when there is no readable native address.
    if offset == BUFFER_ADDRESS_SENTINEL {
        if let Some(obj) = unsafe_obj(args, 1) {
            if let Value::Long(addr) = ctx.get_field_by_name(obj, "address") {
                if addr != 0 {
                    return Ok(Some(Value::Long(addr)));
                }
            }
        }
        return Ok(Some(Value::Long(1)));
    }
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(value) = unsafe_static_get(ctx, offset) {
                return Ok(Some(match value {
                    Value::Long(v) => Value::Long(v),
                    Value::Int(v) => Value::Long(v as i64),
                    _ => Value::Long(0),
                }));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_long_shard(offset);
            return Ok(Some(Value::Long(map.get(&key).copied().unwrap_or(0))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(match synthetic_get(ctx, obj, offset) {
            Value::Long(v) => Value::Long(v),
            Value::Int(v) => Value::Long(v as i64),
            _ => Value::Long(0),
        }));
    }
    let v = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => ctx.get_array_element(obj, idx),
            None => Value::Long(0),
        }
    } else {
        // Wave 8c: an offset INSIDE a field (byte field offsets) reads its
        // remaining bytes.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the zero-extended bytes.
            (slot, k) if k != 0 => {
                Value::Long(unsafe_sub_field_load(&*ctx, obj, slot, k, 8) as i64)
            }
            (slot, _) => ctx.get_field(obj, slot),
        }
    };
    // Coerce non-Long values to Long(0) so stack type stays valid; PD0$4's
    // `lstore` would otherwise see an Object and mis-interpret the slot.
    match v {
        Value::Long(_) => Ok(Some(v)),
        Value::Int(i) => Ok(Some(Value::Long(i as i64))),
        // JIT round 20 wave 7 (lane huntDisp): a `getLong` AT a `double`
        // field's own offset is the field's bits (HotSpot reads its 8
        // bytes), not 0 (`R20HuntDispUnsafeField` `pun.*`).
        Value::Double(d) => Ok(Some(Value::Long(d.to_bits() as i64))),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub(crate) fn native_unsafe_put_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Long(i) = val { i } else { 0 };
            if unsafe_static_put(ctx, offset, Value::Long(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_long_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field replaces its remaining bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 8, bits);
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

fn native_unsafe_allocate_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Unsafe this, args[1] = Class mirror
    // A null class. HotSpot 25.0.4+7 SIGSEGVs in `Unsafe_AllocateInstance`
    // rather than refusing (measured -- it is why the null-argument rows of
    // this lane run one call per process), so again the oracle has no answer
    // and the choice is ours. CratonVM returned a null REFERENCE, which moves
    // the failure to the caller's next dereference and names nothing.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some("Unsafe.allocateInstance: null class".to_string()),
        }
        .into());
    }
    // MEASURED, HotSpot 25.0.4+7: an interface, an abstract class, a primitive
    // class, `void` and an array class are each an `InstantiationException`.
    // CratonVM allocated something for all five. An "instance" of an interface
    // is the fabricated-receiver shape this whole campaign exists to remove --
    // it is handed back with the interface's own name and fails arbitrarily far
    // away, which is exactly what Phase 1 was about.
    let Some(Value::Object(Some(class_obj))) = args.get(1).copied() else {
        return Ok(Some(Value::Object(None)));
    };
    // J8 (round 15 wave 5, lane jdkonly7): the mirror is resolved to its class
    // id ONCE (every constructor `MethodHandle` allocates through here). This
    // resolved it three times: inside `mirror_class_name`, for the flag test
    // and again for the allocation. The name is `mirror_class_name`'s answer
    // exactly -- `class_name_of_id` of the resolved id, its slot-1 reading
    // only for a mirror the reverse map does not know.
    let direct_cid = ctx.class_id_from_mirror(class_obj);
    let cid_opt = direct_cid.or_else(|| match ctx.get_field(class_obj, 0) {
        Value::Int(c) if c >= 0 => Some(cratonvm_types::ClassId::new(c as u32)),
        _ => None,
    });
    let mirror_name = match direct_cid {
        Some(cid) => ctx.class_name_of_id(cid),
        None => crate::lang_class::mirror_class_name(ctx, class_obj),
    };
    if let Some(name) = mirror_name.as_ref() {
        let is_primitive = matches!(
            name.as_str(),
            "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void"
        );
        if is_primitive {
            return unsafe_instantiation_exception(ctx, None);
        }
        if name.starts_with('[') {
            // An array `Klass` fails `check_valid_for_instantiation`
            // (lane jdkonly6: named, `[I` / `[Ljava.lang.String;`).
            let external = unsafe_allocate_instance_checks().then(|| name.class_text(&*ctx));
            return unsafe_instantiation_exception(ctx, external.as_deref());
        }
    }
    let Some(class_id) = cid_opt else {
        return Ok(Some(Value::Object(None)));
    };
    let ACC_INTERFACE = cratonvm_types::access_flags::ACC_INTERFACE;
    let ACC_ABSTRACT = cratonvm_types::access_flags::ACC_ABSTRACT;
    let flags = ctx.class_access_flags(class_id);
    // Round 15 wave 4, lane jdkonly6: HotSpot's
    // `InstanceKlass::check_valid_for_instantiation` names the class
    // (`THROW_MSG(.., external_name())`) for an interface / abstract
    // class, and refuses `java.lang.Class` itself with an
    // `IllegalAccessException` naming it; a class `Klass` that does not
    // exist (a primitive) throws the bare exception above. Both
    // answers were CratonVM's bare `InstantiationException` / an
    // allocated `Class`. `CRATONVM_UNSAFE_ALLOCATE_INSTANCE_CHECKS=0`
    // restores them (read on the refusal path only).
    let refusal = flags & (ACC_INTERFACE | ACC_ABSTRACT) != 0;
    let class_itself = !refusal
        && mirror_name.as_deref() == Some("java/lang/Class")
        && unsafe_allocate_instance_checks();
    if refusal || class_itself {
        let external = unsafe_allocate_instance_checks()
            .then(|| ctx.class_name_of_id(class_id))
            .flatten()
            .map(|name| name.class_text(&*ctx));
        if class_itself {
            return Err(RuntimeError::IllegalAccessException {
                message: external.unwrap_or_else(|| "java.lang.Class".to_string()),
            }
            .into());
        }
        return unsafe_instantiation_exception(ctx, external.as_deref());
    }
    // Allocate without calling `<init>`. Round 15 wave 4, lane jdkonly6
    // (`r15w4-jdkonly5-native-allocation-skips-class-init-FIXED-20260930.md`, JO5-1):
    // through the one native-instantiate funnel, which initializes the class
    // first as HotSpot's `InstanceKlass::allocate_instance(java_class)` does
    // (`<clinit>` side effects are seen by the first allocation; a failing
    // `<clinit>` is the answer -- `ExceptionInInitializerError`, then
    // `NoClassDefFoundError`), and allocates the mirror's EXACT class. This
    // re-resolved the class BY NAME (`allocate_instance`, a class-manager
    // write lock per call), which answers the first same-named definer, and
    // never initialized. `DirectMethodHandle.allocateInstance` (every
    // constructor method handle) relies on the initialization.
    //
    // Interpreter round i2 wave 58, lane L3: an uninitialized class is
    // initialized first, its failure listed under the JDK frames HotSpot
    // raises it in (`allocate_instance_initialize`).
    if !ctx.is_class_initialized(class_id) {
        allocate_instance_initialize(ctx, args.first().copied(), class_id)?;
    }
    // zr z8/nr (`zgc-zr7rv6-near-full-floor-runs-the-heap-dry-before-non-collecting-native-doors-20261013.md`):
    // on every collector (ZGC; Generational and G1 since gci i48/zl2) a
    // refused allocation collects before the error, as HotSpot's
    // `Unsafe_AllocateInstance` does (every constructor `MethodHandle` and
    // `Constructor.newInstance` allocate here). The native holds no heap
    // reference from here on (`args` is not read again), so the ladder's
    // collection cannot dangle anything. `CRATONVM_ZGC_NATIVE_DOORS_COLLECT=0`
    // keeps the one-attempt `instantiate`.
    if ctx.native_doors_collect_on_refusal() {
        if let Some(obj) = ctx.try_instantiate_no_constructor(class_id)? {
            return Ok(Some(Value::Object(Some(obj))));
        }
        let made = crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
            ctx.try_instantiate_no_constructor(class_id).transpose()
        });
        if let Some(made) = made {
            return Ok(Some(Value::Object(Some(made?))));
        }
        // Every rung spent: the one-attempt door below raises the error.
    }
    let built = ctx.instantiate(
        class_id,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    )?;
    Ok(Some(Value::Object(built)))
}

/// `Unsafe.allocateInstance`'s initialization of `class_id`, its failure
/// listed as HotSpot raises it: inside `jdk.internal.misc.Unsafe
/// .allocateInstance (Native Method)`, under `sun.misc.Unsafe.allocateInstance`
/// when the receiver is a `sun.misc.Unsafe` (measured, JDK 25.0.3). Interpreter
/// round i2 wave 58, lane L3.
#[cold]
#[inline(never)]
fn allocate_instance_initialize(
    ctx: &mut dyn NativeContext,
    receiver: Option<Value>,
    class_id: cratonvm_types::ClassId,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const DESC: &str = "(Ljava/lang/Class;)Ljava/lang/Object;";
    let sun_misc = match receiver {
        Some(Value::Object(Some(r))) => {
            ctx.class_name_of_id(ctx.class_id_of_object(r)).as_deref() == Some("sun/misc/Unsafe")
        }
        _ => false,
    };
    let mut hops = Vec::with_capacity(2);
    if sun_misc {
        hops.push(ServedRaiseHop {
            class: "sun/misc/Unsafe",
            name: "allocateInstance",
            descriptor: DESC,
            site: ServedRaiseHopSite::LastCall {
                name: "allocateInstance",
                descriptor: Some(DESC),
            },
        });
    }
    hops.push(ServedRaiseHop {
        class: "jdk/internal/misc/Unsafe",
        name: "allocateInstance",
        descriptor: DESC,
        site: ServedRaiseHopSite::Raise,
    });
    ctx.initialize_class_served(
        class_id,
        ServedRaiseChain {
            throwable_class: "java/lang/ExceptionInInitializerError",
            hops,
        },
    )
}

/// `CRATONVM_UNSAFE_ALLOCATE_INSTANCE_CHECKS` (default on; round 15 wave 4,
/// lane jdkonly6): `Unsafe.allocateInstance`'s refusals carry HotSpot's
/// message and refuse `java.lang.Class`. Read only on a refusal path (and once
/// per call naming `java.lang.Class`), never on an ordinary allocation.
fn unsafe_allocate_instance_checks() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_UNSAFE_ALLOCATE_INSTANCE_CHECKS")
}

pub(crate) fn native_unsafe_fence(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    Ok(None)
}

fn native_unsafe_park(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe, isAbsolute(bool), time(long)]
    // JDK spec: if interrupted, park returns immediately (no exception, flag
    // NOT cleared). Wave 79, lane L6: that return is taken inside the park
    // door below (`park_with_state` returns first thing on the flag), so the
    // `jdk.ThreadPark` event is posted for it, as HotSpot's `Parker::park`
    // returns inside `Unsafe_Park`'s event.
    let is_absolute = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    // CLUSTER-A: long argument may arrive as Value::Double due to operand-stack
    // tag-loss for category-2 longs (CompactValue::long stores raw i64 untagged,
    // and `to_value()` decodes untagged bits as Double). Recover via bit-reinterpret.
    let time: i64 = match args.get(2) {
        Some(Value::Long(t)) => *t,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(t)) => *t as i64,
        _ => 0,
    };

    // `unsafe_park_timeout` answers `Parker::park`'s `time < 0 || (isAbsolute
    // && time == 0)` early return as `Some(ZERO)` (gci i21/qh); interpreter
    // round i5 wave 102, lane L6 (`L6W102ParkUntilZero`) fixed the same
    // `parkUntil(0)` hang on dev, folded into it at the 2026-10-16 merge.
    let timeout = if is_absolute {
        // time is an absolute deadline in milliseconds since the epoch
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        unsafe_park_timeout(true, time, now_ms)
    } else {
        unsafe_park_timeout(false, time, 0)
    };

    // gci i22/rv (finding 3 of
    // `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): an elapsed or
    // non-positive deadline (`Some(ZERO)`) returns at once through the door
    // (the permit taken, the event posted), as `Parker::park` returns for it.
    // It never reaches `try_yield_virtual_park`, which reads `Some(ZERO)` as
    // `wake_after_nanos: 0` -- the UNTIMED yield -- so a VM-scheduled virtual
    // thread (`CRATONVM_VT_CONTINUATIONS=0`) parked forever on a past deadline.
    if ctx.is_interrupted(false) || unsafe_park_returns_at_once(timeout) {
        unsafe_park_door(ctx, timeout, is_absolute, time);
        return Ok(None);
    }
    if let Some(yielded) = try_yield_virtual_park(ctx, timeout) {
        return yielded;
    }
    // Interpreter round i5 wave 101, lane L2 (`CRATONVM_VTHREAD_COMPILED_LEARNED`,
    // default off): a JDK virtual thread parked here from
    // `VirtualThread.parkOnCarrierThread` because a compiled frame (or a VM
    // door's pin) pinned its yield, where HotSpot unmounts: the scheduler
    // compensates for the carrier, as for a pinned `Object.wait`.
    //
    // gci i17/vt (`CRATONVM_VT_PINNED_PARK_COMPENSATE`, opt-in): a mounted JDK
    // virtual thread parks here only when its yield was refused
    // (`VirtualThread.parkOnCarrierThread`), holding its carrier for the
    // park; the scheduler adds one for the time of it, as `Object.wait`'s
    // native does (`monitor_wait_compensated`). The compensation of
    // b056db72a1 went into `native_unsafe_park_with_blocker`, an overload no
    // supported image declares (unregistered since 2026-08-29), so this,
    // the live `park(ZJ)V`, never compensated: a scheduler whose every
    // carrier holds a pinned park starves its queue. HotSpot 25 does not
    // compensate a pinned park either, but pins far less, hence opt-in.
    // Either switch compensates (merge of dev into the gci round,
    // 2026-10-16); dev's predicate is asked first so its census counts.
    //
    // Interpreter round i5 wave 108, lane L6 (page `i91-L6`, "Wave 108"):
    // the learned arm's compensation only for a park that could starve its
    // carrier's queue -- untimed, or timed past
    // `LEARNED_COMPENSATION_MIN_PARK`. A short timed pinned park ends by
    // itself; compensating it made the scheduler add a carrier per
    // concurrently pinned park (up to its max pool size), and those extra
    // carriers outlive the burst by the pool's keep-alive (30 s), so every
    // later rep ran oversubscribed: a run-to-run bimodal `helpers virtual`
    // row, by reading. HotSpot compensates no pinned park.
    let short_timed = timeout.is_some_and(|t| t <= LEARNED_COMPENSATION_MIN_PARK);
    let compensated = ((!short_timed && ctx.vm_pinned_park_compensates())
        || (ctx.blocks_jdk_carrier()
            && cratonvm_types::flags::runtime_flag_on("CRATONVM_VT_PINNED_PARK_COMPENSATE")))
        && ctx.begin_carrier_compensation();
    unsafe_park_door(ctx, timeout, is_absolute, time);
    if compensated {
        ctx.end_carrier_compensation(true);
    }
    Ok(None)
}

/// The longest timed park `CRATONVM_VTHREAD_COMPILED_LEARNED`'s pinned-park
/// compensation leaves uncompensated (interpreter round i5 wave 108, lane
/// L6; [`native_unsafe_park`]): it ends by itself within this, so it cannot
/// starve the carrier's queue for long.
const LEARNED_COMPENSATION_MIN_PARK: std::time::Duration = std::time::Duration::from_millis(20);

/// The wait of `Unsafe.park(isAbsolute, time)` as HotSpot 25's
/// `Parker::park` reads its arguments (gci i21/qh, page
/// `gci-i21qh-g1-virtual-thread-quiet-hang-park-unpark-audit-FIXED-20261017`):
///
/// * `time < 0`, or an ABSOLUTE `time == 0`: return at once (`Some(ZERO)`;
///   the permit is still taken first by the door, as `Parker::park` takes
///   `_counter` before this test). Before i21 an absolute `0` (`parkUntil(0)`)
///   parked FOREVER here -- the relative "0 = untimed" rule was applied to
///   both forms -- and only an `unpark` could end it;
/// * a relative `0`: untimed (`None`);
/// * a relative `time > 0`: `time` nanoseconds;
/// * an absolute `time > 0`: until that epoch millisecond, `Some(ZERO)` once
///   it has passed. `time - now_ms` saturates: before i21 an absolute
///   `Long.MIN_VALUE` overflowed (a debug panic; in release a wrap to a
///   ~292-million-year park).
pub(crate) fn unsafe_park_timeout(
    is_absolute: bool,
    time: i64,
    now_ms: i64,
) -> Option<std::time::Duration> {
    if time < 0 || (is_absolute && time == 0) {
        return Some(std::time::Duration::ZERO);
    }
    if time == 0 {
        return None;
    }
    if is_absolute {
        let remaining = time.saturating_sub(now_ms).max(0) as u64;
        return Some(std::time::Duration::from_millis(remaining));
    }
    Some(std::time::Duration::from_nanos(time as u64))
}

/// gci i22/rv: does [`unsafe_park_timeout`]'s answer return at once (an
/// elapsed or non-positive deadline)?
#[inline]
pub(crate) fn unsafe_park_returns_at_once(timeout: Option<std::time::Duration>) -> bool {
    timeout == Some(std::time::Duration::ZERO)
}

/// The `jdk.ThreadPark` event's `until` field for an ABSOLUTE
/// `Unsafe.park(true, time)`, as HotSpot 25's `Unsafe_Park` posts it: the
/// deadline, except `time == 0`, which it posts as an untimed park
/// (`timeout = until = Long.MIN_VALUE`, its `if (time == 0)` arm comes before
/// the absolute one). gci i22/rv (finding 4 of
/// `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): i21/qh's
/// absolute door posted `until = 0` for `parkUntil(0)`.
#[inline]
pub(crate) fn unsafe_park_absolute_until_field(time: i64) -> i64 {
    if time == 0 {
        i64::MIN
    } else {
        time
    }
}

/// `Unsafe.park`'s Java-level park door (interpreter round i3 wave 79, lane
/// L6): an absolute park carries the caller's deadline for the
/// `jdk.ThreadPark` event's `until` (HotSpot: `timeout = Long.MIN_VALUE`,
/// `until = time`); every other park the relative door. gci i21/qh: every
/// absolute park takes the absolute door, an absolute `0` included (it
/// returns at once, `unsafe_park_timeout`), as `Unsafe_Park` posts it --
/// gci i22/rv: with `until = Long.MIN_VALUE` for it
/// ([`unsafe_park_absolute_until_field`]).
fn unsafe_park_door(
    ctx: &mut dyn NativeContext,
    timeout: Option<std::time::Duration>,
    is_absolute: bool,
    time: i64,
) {
    if is_absolute {
        ctx.park_until_reporting(timeout, unsafe_park_absolute_until_field(time));
    } else if let Some(raw) = unsafe_park_relative_raw_timeout_field(time) {
        ctx.park_relative_reporting_raw(timeout, raw);
    } else {
        ctx.park_reporting_timeout(timeout);
    }
}

/// gci i23/pk (finding 6 of
/// `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): the
/// `jdk.ThreadPark` `timeout` a RELATIVE `Unsafe.park(false, time)` needs
/// carried raw: a negative `time`, which HotSpot 25's `Unsafe_Park` posts as
/// is (`timeout = time`) and which [`unsafe_park_timeout`]'s `Some(ZERO)`
/// cannot carry. `None` for `time >= 0`: `0` posts as untimed and a positive
/// time as its nanoseconds, both already right through the `Duration`.
#[inline]
pub(crate) fn unsafe_park_relative_raw_timeout_field(time: i64) -> Option<i64> {
    (time < 0).then_some(time)
}

fn native_unsafe_unpark(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe, thread_obj]
    if let Some(Value::Object(Some(thread_obj))) = args.get(1) {
        ctx.unpark(*thread_obj);
    } else {
    }
    Ok(None)
}

/// `Unsafe.getAndAddInt`; an int-width add over another element type is a
/// raw-bytes read-modify-write ([`unsafe_wide_raw_rmw`]).
pub(crate) fn native_unsafe_get_and_add_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Int, 4)
    {
        let d = unsafe_wide_arg_bits(args, 3, 4);
        if let Some(old) = unsafe_wide_raw_rmw(ctx, obj, off, 4, &|cur| cur.wrapping_add(d)) {
            return Ok(Some(Value::Int(old as u32 as i32)));
        }
    }
    native_unsafe_get_and_add_int_element(ctx, args)
}

/// [`native_unsafe_get_and_add_int`] without the raw-bytes arm (the
/// byte/short adds' fallback).
pub(crate) fn native_unsafe_get_and_add_int_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), delta(int)]
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Add, 4) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Add, 4) {
        return Ok(Some(v));
    }
    let offset = unsafe_offset(args, 2);
    let delta = match args.get(3) {
        Some(Value::Int(d)) => *d,
        _ => 0,
    };
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(old) = unsafe_static_get_and_add_int(ctx, offset, delta) {
                return Ok(Some(Value::Int(old)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_int_shard(offset);
            let slot = map.entry((ctx.vm_identity(), offset)).or_insert(0);
            let old = *slot;
            *slot = old.wrapping_add(delta);
            return Ok(Some(Value::Int(old)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        for attempt in 0.. {
            let current = synthetic_get(ctx, obj, offset);
            if let Value::Int(old) = current {
                let new_val = Value::Int(old.wrapping_add(delta));
                if synthetic_cas(ctx, obj, offset, current, new_val) {
                    return Ok(Some(Value::Int(old)));
                }
            } else {
                return Ok(Some(Value::Int(0)));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return 0 without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        // ATOMIC -- see the note in `native_unsafe_get_and_set_object`. A
        // read-modify-write on an array element loses concurrent increments;
        // the field arm below has always been a CAS loop.
        for attempt in 0.. {
            let current = ctx.get_array_element(obj, idx);
            let Value::Int(old) = current else {
                return Ok(Some(Value::Int(0)));
            };
            if ctx.compare_and_swap_field(obj, idx, current, Value::Int(old.wrapping_add(delta))) {
                return Ok(Some(Value::Int(old)));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    // CAS loop with bounded retries and yield on contention
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if let Value::Int(old) = current {
            let new_val = Value::Int(old.wrapping_add(delta));
            if ctx.compare_and_swap_field(obj, offset, current, new_val) {
                return Ok(Some(Value::Int(old)));
            }
        } else {
            return Ok(Some(Value::Int(0)));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

fn native_unsafe_get_and_set_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), new_val(int)]
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Set, 4) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Set, 4) {
        return Ok(Some(v));
    }
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Int, 4)
    {
        let nv = unsafe_wide_arg_bits(args, 3, 4);
        if let Some(old) = unsafe_wide_raw_rmw(ctx, obj, off, 4, &|_| nv) {
            return Ok(Some(Value::Int(old as u32 as i32)));
        }
    }
    let offset = unsafe_offset(args, 2);
    let new_val = args.get(3).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let nv = if let Value::Int(n) = new_val { n } else { 0 };
            if let Some(prev) = unsafe_static_get_and_set_int(ctx, offset, nv) {
                return Ok(Some(Value::Int(prev)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_int_shard(offset);
            let prev = map.insert((ctx.vm_identity(), offset), nv).unwrap_or(0);
            return Ok(Some(Value::Int(prev)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let prev = synthetic_get(ctx, obj, offset);
        synthetic_put(ctx, obj, offset, new_val);
        return Ok(Some(prev));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return Int(0) without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        // ATOMIC, and it was not until 2026-09-10. A plain read-then-write
        // here is a lost update: two threads reading the same slot both see a
        // non-null `prev` and both report having taken it. The FIELD arm below
        // has always been a CAS retry loop; this arm was a bare
        // `get_array_element` + `set_array_element` pair, so the atomicity of
        // `getAndSet*` depended on whether the receiver happened to be an
        // array.
        //
        // The cost is not theoretical and not confined to `Unsafe`'s own
        // callers. `java.util.concurrent.ForkJoinPool$WorkQueue` claims a
        // queued task with exactly this call --
        // `U.getAndSetReference(a, slotOffset(k), null)` -- so a doubled claim
        // is a task EXECUTED TWICE. Measured with `apps/probes/L5CasRace.java`
        // on the Azure host: 2000 slots, 4 threads, HotSpot 2000 claims and
        // this VM **2035**; and with the real `ForkJoinPool` bytecode running
        // (`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ForkJoinPool,
        // java/util/concurrent/ForkJoinTask`)
        // `apps/probes/L5CountedCompleter.java` read 64..128 leaves for a
        // 64-leaf tree across eight runs. That is lane 5's
        // `RJdkForkJoin  AssertionError: CountedCompleter leaves: 128`.
        for attempt in 0.. {
            let prev = ctx.get_array_element(obj, idx);
            if ctx.compare_and_swap_field(obj, idx, prev, new_val) {
                return Ok(Some(prev));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if ctx.compare_and_swap_field(obj, offset, current, new_val) {
            return Ok(Some(current));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

/// `Unsafe.getFloat(Object, long)` with the NULL-BASE arm routed off-heap.
///
/// A null base means an absolute address, and `getFloat(long)` is literally
/// `getFloat(null, address)` in the JDK -- so a write through one spelling has
/// to be visible through the other. It was not: the null-base arm of
/// `native_unsafe_get_float` reads a private static-field side map keyed by the
/// address, which round-trips perfectly WITHIN that door and shares no storage
/// with the arena the 1-arg form uses. MEASURED, HotSpot 25.0.4+7:
///
///   putFloat(null, addr, 1.5f) then getFloat(addr)   HotSpot 1.5   CratonVM 0.0
///   putFloat(addr, 2.5f) then getFloat(null, addr)   HotSpot 2.5   CratonVM 0.0
///
/// int, long, byte, short and char already have `_mb` handlers that route a
/// null base through `copy_from_native_memory`; float and double were the two
/// widths that never got one. That is the same population as
/// `a-null-base-unsafe-access-means-off-heap-not-a-static-field` (2026-08-24) --
/// which fixed the ONE-ARG forms and left the two-arg null-base forms behind.
pub(crate) fn native_unsafe_get_float_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if let Some(v) = unsafe_field_view_get(&*ctx, args, b'F', false) {
        return Ok(Some(v));
    }
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let mut b = [0u8; 4];
        if ctx.copy_from_native_memory(addr, &mut b) {
            return Ok(Some(Value::Float(f32::from_le_bytes(b))));
        }
    }
    // SBR3 ffmperf: the raw bits of any other primitive array (`getFloat(int[]
    // .., off)` is the bit pattern, not an element converted).
    if let Some(obj) = unsafe_obj(args, 1) {
        if unsafe_array_access_is_raw(
            ctx,
            obj,
            cratonvm_types::ArrayElementType::Float,
            unsafe_offset(args, 2),
            4,
        ) {
            let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
            if unsafe_read_bytes_into(ctx, obj, unsafe_offset(args, 2), 4, &mut b) {
                return Ok(Some(Value::Float(f32::from_le_bytes([
                    b[0], b[1], b[2], b[3],
                ]))));
            }
        }
    }
    native_unsafe_get_float(ctx, args)
}

/// SBR3 lane ffmperf: is `obj` a primitive array whose element type is not
/// `natural`, or whose `width`-byte access at `offset` straddles elements? Then
/// an `Unsafe.get*` / `put*` of that width is a read/write of raw bytes.
fn unsafe_array_access_is_raw(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    natural: cratonvm_types::ArrayElementType,
    offset: usize,
    width: usize,
) -> bool {
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let et = ctx.heap_element_type_of(obj);
    if et == cratonvm_types::ArrayElementType::Reference {
        return false;
    }
    et != natural || (offset >= 16 && (offset - 16) % width != 0)
}

/// Write half of [`native_unsafe_get_float_mb`].
pub(crate) fn native_unsafe_put_float_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if unsafe_field_view_put(ctx, args, b'F') {
        return Ok(None);
    }
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let bits = match args.get(3) {
            Some(Value::Float(f)) => f.to_bits(),
            Some(Value::Int(i)) => *i as u32,
            _ => 0,
        };
        if ctx.copy_to_native_memory(addr, &bits.to_le_bytes()) {
            return Ok(None);
        }
    }
    // SBR3 ffmperf: the raw store into any other primitive array.
    if let Some(obj) = unsafe_obj(args, 1) {
        if unsafe_array_access_is_raw(
            ctx,
            obj,
            cratonvm_types::ArrayElementType::Float,
            unsafe_offset(args, 2),
            4,
        ) {
            let bits = match args.get(3) {
                Some(Value::Float(f)) => f.to_bits(),
                Some(Value::Int(i)) => *i as u32,
                _ => 0,
            };
            if unsafe_array_write_bytes(ctx, obj, unsafe_offset(args, 2), &bits.to_le_bytes()) {
                return Ok(None);
            }
        }
    }
    native_unsafe_put_float(ctx, args)
}

/// `Unsafe.getDouble(Object, long)` with the null-base arm routed off-heap.
/// See [`native_unsafe_get_float_mb`].
pub(crate) fn native_unsafe_get_double_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if let Some(v) = unsafe_field_view_get(&*ctx, args, b'D', false) {
        return Ok(Some(v));
    }
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let mut b = [0u8; 8];
        if ctx.copy_from_native_memory(addr, &mut b) {
            return Ok(Some(Value::Double(f64::from_le_bytes(b))));
        }
    }
    // SBR3 ffmperf: see `native_unsafe_get_float_mb`.
    if let Some(obj) = unsafe_obj(args, 1) {
        if unsafe_array_access_is_raw(
            ctx,
            obj,
            cratonvm_types::ArrayElementType::Double,
            unsafe_offset(args, 2),
            8,
        ) {
            let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
            if unsafe_read_bytes_into(ctx, obj, unsafe_offset(args, 2), 8, &mut b) {
                return Ok(Some(Value::Double(f64::from_le_bytes(b))));
            }
        }
    }
    native_unsafe_get_double(ctx, args)
}

/// Write half of [`native_unsafe_get_double_mb`].
pub(crate) fn native_unsafe_put_double_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if unsafe_field_view_put(ctx, args, b'D') {
        return Ok(None);
    }
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let bits = match args.get(3) {
            Some(Value::Double(d)) => d.to_bits(),
            Some(Value::Long(l)) => *l as u64,
            _ => 0,
        };
        if ctx.copy_to_native_memory(addr, &bits.to_le_bytes()) {
            return Ok(None);
        }
    }
    // SBR3 ffmperf: see `native_unsafe_put_float_mb`.
    if let Some(obj) = unsafe_obj(args, 1) {
        if unsafe_array_access_is_raw(
            ctx,
            obj,
            cratonvm_types::ArrayElementType::Double,
            unsafe_offset(args, 2),
            8,
        ) {
            let bits = match args.get(3) {
                Some(Value::Double(d)) => d.to_bits(),
                Some(Value::Long(l)) => *l as u64,
                _ => 0,
            };
            if unsafe_array_write_bytes(ctx, obj, unsafe_offset(args, 2), &bits.to_le_bytes()) {
                return Ok(None);
            }
        }
    }
    native_unsafe_put_double(ctx, args)
}

pub(crate) fn native_unsafe_get_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            return Ok(Some(match unsafe_static_get(ctx, offset) {
                Some(Value::Float(f)) => Value::Float(f),
                Some(Value::Int(bits)) => Value::Float(f32::from_bits(bits as u32)),
                _ => Value::Float(0.0),
            }));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let val = synthetic_get(ctx, obj, offset);
        return Ok(Some(match val {
            Value::Float(_) => val,
            Value::Int(bits) => Value::Float(f32::from_bits(bits as u32)),
            _ => Value::Float(0.0),
        }));
    }
    // An ARRAY base is an ELEMENT access, not a field access — the same screen
    // `native_unsafe_put_int` / `native_unsafe_get_long` have always carried,
    // and the four float/double natives were the only ones without it.
    //
    // `Unsafe.{get,put}{Float,Double}(Object, long, …)` over an array reached
    // `ctx.{get,set}_field(obj, offset)` with the BYTE OFFSET as a slot index.
    // An array mirrors its LENGTH into `num_slots`, so the index check admits
    // it, and the accessor then addresses a 16-byte `Value` cell at
    // `HEADER_SIZE + offset * 16` — for Hazelcast's
    // `UnsafeUtil.checkUnsafeInstance`, a `putFloat(new byte[32], 16, 3f)` that
    // computed byte 272 of a 32-byte body, i.e. 240 bytes past the allocation.
    // Found 2026-08-23 by the array-receiver screen at the heap accessors
    // (`corrupt-value-cell-array-receiver-species-CLOSED-20260823`), which is
    // what turned a silent out-of-bounds write into a named producer.
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // The read half of `native_unsafe_put_float`'s byte-addressed write.
        // Restricted to `byte[]`/`boolean[]` on purpose: for a typed array the
        // element accessor is the authority on how that element is stored, and
        // re-assembling it from bytes would be a second, unvalidated decoding.
        if matches!(
            ctx.heap_element_type_of(obj),
            cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
        ) {
            match unsafe_read_bytes_from_array(ctx, obj, offset, 4) {
                Some(b) => Value::Float(f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                None => Value::Float(0.0),
            }
        } else {
            match unsafe_checked_array_index(ctx, obj, offset) {
                Some(idx) => ctx.get_array_element(obj, idx),
                None => Value::Float(0.0),
            }
        }
    } else {
        // Wave 8c: an offset INSIDE a field reads its bytes as the float.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the low 32 bits of the zero-extended bytes.
            (slot, k) if k != 0 => {
                Value::Int(unsafe_sub_field_load(&*ctx, obj, slot, k, 4) as u32 as i32)
            }
            (slot, _) => ctx.get_field(obj, slot),
        }
    };
    match val {
        Value::Float(_) => Ok(Some(val)),
        Value::Int(bits) => Ok(Some(Value::Float(f32::from_bits(bits as u32)))),
        _ => Ok(Some(Value::Float(0.0))),
    }
}

pub(crate) fn native_unsafe_put_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Float(0.0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let _ = unsafe_static_put(ctx, offset, val);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    // An ARRAY base is an ELEMENT access, not a field access — the same screen
    // `native_unsafe_put_int` / `native_unsafe_get_long` have always carried,
    // and the four float/double natives were the only ones without it.
    //
    // `Unsafe.{get,put}{Float,Double}(Object, long, …)` over an array reached
    // `ctx.{get,set}_field(obj, offset)` with the BYTE OFFSET as a slot index.
    // An array mirrors its LENGTH into `num_slots`, so the index check admits
    // it, and the accessor then addresses a 16-byte `Value` cell at
    // `HEADER_SIZE + offset * 16` — for Hazelcast's
    // `UnsafeUtil.checkUnsafeInstance`, a `putFloat(new byte[32], 16, 3f)` that
    // computed byte 272 of a 32-byte body, i.e. 240 bytes past the allocation.
    // Found 2026-08-23 by the array-receiver screen at the heap accessors
    // (`corrupt-value-cell-array-receiver-species-CLOSED-20260823`), which is
    // what turned a silent out-of-bounds write into a named producer.
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // A `byte[]` base is a BYTE-ADDRESSED write of the full 4-byte value,
        // not a one-element store. `unsafe_multibyte_put!` says the same thing
        // for short/char/int/long; float and double were not wired to it, and
        // routing them to `set_array_element` alone would truncate a float to
        // one byte -- no longer an out-of-bounds write, but still not HotSpot's
        // answer. This is the idiom `Bits.writeIntL([BII)` (one frame below
        // Hazelcast's probe in the stack that found this) depends on.
        let bits = match val {
            Value::Float(f) => f.to_bits(),
            Value::Int(i) => i as u32,
            _ => 0,
        };
        if unsafe_write_bytes_to_byte_array(ctx, obj, offset, &bits.to_le_bytes()) {
            return Ok(None);
        }
        // Any other primitive array: the offset names one typed element.
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // An out-of-range offset is a no-op, matching the int/long siblings:
        // never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field replaces four of its bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 4, bits);
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

pub(crate) fn native_unsafe_get_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            return Ok(Some(match unsafe_static_get(ctx, offset) {
                Some(Value::Double(d)) => Value::Double(d),
                Some(Value::Long(bits)) => Value::Double(f64::from_bits(bits as u64)),
                _ => Value::Double(0.0),
            }));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let val = synthetic_get(ctx, obj, offset);
        return Ok(Some(match val {
            Value::Double(_) => val,
            Value::Long(bits) => Value::Double(f64::from_bits(bits as u64)),
            _ => Value::Double(0.0),
        }));
    }
    // An ARRAY base is an ELEMENT access, not a field access — the same screen
    // `native_unsafe_put_int` / `native_unsafe_get_long` have always carried,
    // and the four float/double natives were the only ones without it.
    //
    // `Unsafe.{get,put}{Float,Double}(Object, long, …)` over an array reached
    // `ctx.{get,set}_field(obj, offset)` with the BYTE OFFSET as a slot index.
    // An array mirrors its LENGTH into `num_slots`, so the index check admits
    // it, and the accessor then addresses a 16-byte `Value` cell at
    // `HEADER_SIZE + offset * 16` — for Hazelcast's
    // `UnsafeUtil.checkUnsafeInstance`, a `putFloat(new byte[32], 16, 3f)` that
    // computed byte 272 of a 32-byte body, i.e. 240 bytes past the allocation.
    // Found 2026-08-23 by the array-receiver screen at the heap accessors
    // (`corrupt-value-cell-array-receiver-species-CLOSED-20260823`), which is
    // what turned a silent out-of-bounds write into a named producer.
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // Byte-addressed over a `byte[]` only — see `native_unsafe_get_float`.
        if matches!(
            ctx.heap_element_type_of(obj),
            cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
        ) {
            match unsafe_read_bytes_from_array(ctx, obj, offset, 8) {
                Some(b) => Value::Double(f64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ])),
                None => Value::Double(0.0),
            }
        } else {
            match unsafe_checked_array_index(ctx, obj, offset) {
                Some(idx) => ctx.get_array_element(obj, idx),
                None => Value::Double(0.0),
            }
        }
    } else {
        // Wave 8c: an offset INSIDE a field reads its bytes as the double.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the zero-extended bytes.
            (slot, k) if k != 0 => {
                Value::Long(unsafe_sub_field_load(&*ctx, obj, slot, k, 8) as i64)
            }
            (slot, _) => ctx.get_field(obj, slot),
        }
    };
    match val {
        Value::Double(_) => Ok(Some(val)),
        Value::Long(bits) => Ok(Some(Value::Double(f64::from_bits(bits as u64)))),
        _ => Ok(Some(Value::Double(0.0))),
    }
}

pub(crate) fn native_unsafe_put_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Double(0.0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let _ = unsafe_static_put(ctx, offset, val);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    // An ARRAY base is an ELEMENT access, not a field access — the same screen
    // `native_unsafe_put_int` / `native_unsafe_get_long` have always carried,
    // and the four float/double natives were the only ones without it.
    //
    // `Unsafe.{get,put}{Float,Double}(Object, long, …)` over an array reached
    // `ctx.{get,set}_field(obj, offset)` with the BYTE OFFSET as a slot index.
    // An array mirrors its LENGTH into `num_slots`, so the index check admits
    // it, and the accessor then addresses a 16-byte `Value` cell at
    // `HEADER_SIZE + offset * 16` — for Hazelcast's
    // `UnsafeUtil.checkUnsafeInstance`, a `putFloat(new byte[32], 16, 3f)` that
    // computed byte 272 of a 32-byte body, i.e. 240 bytes past the allocation.
    // Found 2026-08-23 by the array-receiver screen at the heap accessors
    // (`corrupt-value-cell-array-receiver-species-CLOSED-20260823`), which is
    // what turned a silent out-of-bounds write into a named producer.
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // Byte-addressed over a `byte[]`, one typed element otherwise — see
        // `native_unsafe_put_float`.
        let bits = match val {
            Value::Double(d) => d.to_bits(),
            Value::Long(l) => l as u64,
            _ => 0,
        };
        if unsafe_write_bytes_to_byte_array(ctx, obj, offset, &bits.to_le_bytes()) {
            return Ok(None);
        }
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
    } else {
        // Wave 8c: an offset INSIDE a field replaces its remaining bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 8, bits);
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

fn native_unsafe_get_and_add_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Add, 8) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Add, 8) {
        return Ok(Some(v));
    }
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Long, 8)
    {
        let d = unsafe_wide_arg_bits(args, 3, 8);
        if let Some(old) = unsafe_wide_raw_rmw(ctx, obj, off, 8, &|cur| cur.wrapping_add(d)) {
            return Ok(Some(Value::Long(old as i64)));
        }
    }
    let delta = match args.get(3) {
        Some(Value::Long(d)) => *d,
        _ => 0,
    };
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(old) = unsafe_static_get_and_add_long(ctx, offset, delta) {
                return Ok(Some(Value::Long(old)));
            }
            // Static-field semantics (null receiver). Maintain a per-offset
            // counter so callers like Thread$ThreadIdentifiers.next() get
            // monotonically-increasing values rather than a VM panic.
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_long_shard(offset);
            let slot = map.entry((ctx.vm_identity(), offset)).or_insert(0);
            let old = *slot;
            *slot = old.wrapping_add(delta);
            return Ok(Some(Value::Long(old)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        for attempt in 0.. {
            let current = synthetic_get(ctx, obj, offset);
            if let Value::Long(old) = current {
                let new_val = Value::Long(old.wrapping_add(delta));
                if synthetic_cas(ctx, obj, offset, current, new_val) {
                    return Ok(Some(Value::Long(old)));
                }
            } else {
                return Ok(Some(Value::Long(0)));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return 0 without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Long(0))),
        };
        // ATOMIC -- see the note in `native_unsafe_get_and_set_object`.
        for attempt in 0.. {
            let current = ctx.get_array_element(obj, idx);
            let Value::Long(old) = current else {
                return Ok(Some(Value::Long(0)));
            };
            if ctx.compare_and_swap_field(obj, idx, current, Value::Long(old.wrapping_add(delta))) {
                return Ok(Some(Value::Long(old)));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if let Value::Long(old) = current {
            let new_val = Value::Long(old.wrapping_add(delta));
            if ctx.compare_and_swap_field(obj, offset, current, new_val) {
                return Ok(Some(Value::Long(old)));
            }
        } else {
            return Ok(Some(Value::Long(0)));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

fn native_unsafe_get_and_set_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Set, 8) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Set, 8) {
        return Ok(Some(v));
    }
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Long, 8)
    {
        let nv = unsafe_wide_arg_bits(args, 3, 8);
        if let Some(old) = unsafe_wide_raw_rmw(ctx, obj, off, 8, &|_| nv) {
            return Ok(Some(Value::Long(old as i64)));
        }
    }
    let offset = unsafe_offset(args, 2);
    let new_val = args.get(3).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let nv = if let Value::Long(n) = new_val { n } else { 0 };
            if let Some(prev) = unsafe_static_get_and_set_long(ctx, offset, nv) {
                return Ok(Some(Value::Long(prev)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_long_shard(offset);
            let prev = map.insert((ctx.vm_identity(), offset), nv).unwrap_or(0);
            return Ok(Some(Value::Long(prev)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let prev = synthetic_get(ctx, obj, offset);
        synthetic_put(ctx, obj, offset, new_val);
        return Ok(Some(prev));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return Long(0) without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Long(0))),
        };
        // ATOMIC, and it was not until 2026-09-10. A plain read-then-write
        // here is a lost update: two threads reading the same slot both see a
        // non-null `prev` and both report having taken it. The FIELD arm below
        // has always been a CAS retry loop; this arm was a bare
        // `get_array_element` + `set_array_element` pair, so the atomicity of
        // `getAndSet*` depended on whether the receiver happened to be an
        // array.
        //
        // The cost is not theoretical and not confined to `Unsafe`'s own
        // callers. `java.util.concurrent.ForkJoinPool$WorkQueue` claims a
        // queued task with exactly this call --
        // `U.getAndSetReference(a, slotOffset(k), null)` -- so a doubled claim
        // is a task EXECUTED TWICE. Measured with `apps/probes/L5CasRace.java`
        // on the Azure host: 2000 slots, 4 threads, HotSpot 2000 claims and
        // this VM **2035**; and with the real `ForkJoinPool` bytecode running
        // (`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ForkJoinPool,
        // java/util/concurrent/ForkJoinTask`)
        // `apps/probes/L5CountedCompleter.java` read 64..128 leaves for a
        // 64-leaf tree across eight runs. That is lane 5's
        // `RJdkForkJoin  AssertionError: CountedCompleter leaves: 128`.
        for attempt in 0.. {
            let prev = ctx.get_array_element(obj, idx);
            if ctx.compare_and_swap_field(obj, idx, prev, new_val) {
                return Ok(Some(prev));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if ctx.compare_and_swap_field(obj, offset, current, new_val) {
            return Ok(Some(current));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

fn native_unsafe_get_and_set_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let nv = if let Value::Object(n) = new_val {
                n
            } else {
                None
            };
            if let Some(prev) = unsafe_static_get_and_set_object(ctx, offset, nv) {
                return Ok(Some(Value::Object(prev)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let mut map = lock_static_obj_shard(offset);
            let prev = map.insert(key, nv).unwrap_or(None);
            return Ok(Some(Value::Object(prev)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let prev = synthetic_get(ctx, obj, offset);
        synthetic_put(ctx, obj, offset, new_val);
        return Ok(Some(prev));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return null without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Object(None))),
        };
        // ATOMIC, and it was not until 2026-09-10. A plain read-then-write
        // here is a lost update: two threads reading the same slot both see a
        // non-null `prev` and both report having taken it. The FIELD arm below
        // has always been a CAS retry loop; this arm was a bare
        // `get_array_element` + `set_array_element` pair, so the atomicity of
        // `getAndSet*` depended on whether the receiver happened to be an
        // array.
        //
        // The cost is not theoretical and not confined to `Unsafe`'s own
        // callers. `java.util.concurrent.ForkJoinPool$WorkQueue` claims a
        // queued task with exactly this call --
        // `U.getAndSetReference(a, slotOffset(k), null)` -- so a doubled claim
        // is a task EXECUTED TWICE. Measured with `apps/probes/L5CasRace.java`
        // on the Azure host: 2000 slots, 4 threads, HotSpot 2000 claims and
        // this VM **2035**; and with the real `ForkJoinPool` bytecode running
        // (`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ForkJoinPool,
        // java/util/concurrent/ForkJoinTask`)
        // `apps/probes/L5CountedCompleter.java` read 64..128 leaves for a
        // 64-leaf tree across eight runs. That is lane 5's
        // `RJdkForkJoin  AssertionError: CountedCompleter leaves: 128`.
        for attempt in 0.. {
            let prev = ctx.get_array_element(obj, idx);
            if ctx.compare_and_swap_field(obj, idx, prev, new_val) {
                return Ok(Some(prev));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot; an offset inside a field names no reference.
    let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
    if k != 0 {
        return Ok(Some(Value::Object(None)));
    }
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if ctx.compare_and_swap_field(obj, offset, current, new_val) {
            return Ok(Some(current));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

/// Read `n` bytes from a primitive array starting at the raw byte offset
/// `byte_off` (an `Unsafe` offset including `ABASE` = 16). Each element is
/// decomposed into native (little-endian) byte order. Returns `None` if the
/// object is not a primitive array or the range is out of bounds.
pub(crate) fn unsafe_array_read_bytes(
    ctx: &dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    byte_off: usize,
    n: usize,
) -> Option<Vec<u8>> {
    if ctx.heap_kind_of(arr) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let et = ctx.heap_element_type_of(arr);
    if et == cratonvm_types::ArrayElementType::Reference {
        return None;
    }
    const ABASE: usize = 16;
    let start = byte_off.checked_sub(ABASE)?;
    let width = array_elem_byte_width(et);
    let len = ctx.array_length(arr);
    let total = len.checked_mul(width)?;
    if start + n > total {
        return None;
    }
    // Perf fast path: for byte/boolean arrays the element width is 1, so the
    // raw byte offset `start` is exactly the element index and each element is
    // a single byte in native (and Java) order. Route through the bulk
    // `read_byte_array_into` NativeContext intrinsic, which the VM overrides
    // with a single `ptr::copy_nonoverlapping`, avoiding a per-element
    // `get_array_element` (Value box + dynamic dispatch) on the hot path.
    // Bounds are already validated above (`start + n <= total == len`, and
    // `width == 1` so `start <= len`); the intrinsic clamps independently as
    // well, so this cannot read past the array.
    if matches!(
        et,
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
    ) {
        debug_assert_eq!(width, 1);
        let mut out = vec![0u8; n];
        let copied = ctx.read_byte_array_into(arr, start, &mut out);
        // `start + n <= len` guarantees the intrinsic copies exactly `n`.
        debug_assert_eq!(copied, n);
        if copied != n {
            return None;
        }
        return Some(out);
    }
    // SBR3 lane ffmperf: every other primitive width is ALSO a contiguous
    // native-endian payload, so the whole range is one `memcpy` through the
    // byte-level bulk read (`MemorySegment.copy(seg, JAVA_INT, .., int[], ..)`
    // and `toArray` reach here through the real `Unsafe.copyMemory`; the loop
    // below paid a `Value` round trip per element, 30x the carrier's bulk
    // copy). A context with no bulk path answers 0 and takes the loop.
    {
        let mut out = vec![0u8; n];
        if n == 0 || ctx.read_primitive_array_bytes(arr, start, &mut out) == n {
            return Some(out);
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut produced = 0usize;
    let mut elem_idx = start / width;
    let mut byte_in_elem = start % width;
    while produced < n {
        let v = ctx.get_array_element(arr, elem_idx);
        let raw: u64 = match et {
            cratonvm_types::ArrayElementType::Float => match v {
                Value::Float(f) => f.to_bits() as u64,
                Value::Int(i) => i as u32 as u64,
                _ => 0,
            },
            cratonvm_types::ArrayElementType::Double => match v {
                Value::Long(l) => l as u64,
                Value::Double(d) => d.to_bits(),
                _ => v.as_long().unwrap_or(0) as u64,
            },
            cratonvm_types::ArrayElementType::Long => match v {
                Value::Long(l) => l as u64,
                _ => v.as_long().unwrap_or(0) as u64,
            },
            _ => (v.as_int().unwrap_or(0) as i64) as u64,
        };
        let elem_bytes = raw.to_le_bytes();
        while byte_in_elem < width && produced < n {
            out.push(elem_bytes[byte_in_elem]);
            byte_in_elem += 1;
            produced += 1;
        }
        byte_in_elem = 0;
        elem_idx += 1;
    }
    Some(out)
}

/// Write a byte stream into a primitive array starting at the raw byte
/// offset `byte_off`. Each destination element is reassembled from native
/// (little-endian) bytes. Partial-element writes preserve the surrounding
/// bytes of the touched element. Returns `false` if `arr` is not a
/// primitive array or the range is out of bounds.
pub(crate) fn unsafe_array_write_bytes(
    ctx: &mut dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    byte_off: usize,
    bytes: &[u8],
) -> bool {
    if ctx.heap_kind_of(arr) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let et = ctx.heap_element_type_of(arr);
    if et == cratonvm_types::ArrayElementType::Reference {
        return false;
    }
    const ABASE: usize = 16;
    let start = match byte_off.checked_sub(ABASE) {
        Some(s) => s,
        None => return false,
    };
    let width = array_elem_byte_width(et);
    let len = ctx.array_length(arr);
    let total = match len.checked_mul(width) {
        Some(t) => t,
        None => return false,
    };
    if start + bytes.len() > total {
        return false;
    }
    // Perf fast path: for byte/boolean arrays width is 1, so `start` is the
    // element index and there are no partial-element (read-modify-write)
    // concerns — every touched byte is a whole element. Route through the bulk
    // `write_byte_array_from` NativeContext intrinsic (VM override =
    // `ptr::copy_nonoverlapping`), avoiding the per-element get/set Value
    // boxing + dynamic dispatch. Bounds are already validated above
    // (`start + bytes.len() <= total == len`, `width == 1`); the intrinsic
    // re-checks `dst_off + src.len() <= array_length` and returns `false`
    // without writing anything on mismatch, preserving the bounds contract.
    if matches!(
        et,
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
    ) {
        debug_assert_eq!(width, 1);
        return ctx.write_byte_array_from(arr, start, bytes);
    }
    // SBR3 lane ffmperf: the bulk twin of `unsafe_array_read_bytes`'s: one
    // `memcpy` into the payload (partial elements keep their other bytes
    // trivially), the read-modify-write loop below only for a context with no
    // bulk path.
    if bytes.is_empty() || ctx.write_primitive_array_bytes(arr, start, bytes) {
        return true;
    }
    let mut consumed = 0usize;
    let mut elem_idx = start / width;
    let mut byte_in_elem = start % width;
    while consumed < bytes.len() {
        // Read-modify-write the touched element so partial writes are safe.
        let cur = ctx.get_array_element(arr, elem_idx);
        let mut elem_bytes = match et {
            cratonvm_types::ArrayElementType::Double => match cur {
                Value::Long(l) => (l as u64).to_le_bytes(),
                Value::Double(d) => d.to_bits().to_le_bytes(),
                _ => 0u64.to_le_bytes(),
            },
            cratonvm_types::ArrayElementType::Long => match cur {
                Value::Long(l) => (l as u64).to_le_bytes(),
                _ => 0u64.to_le_bytes(),
            },
            // A `float[]` element is a `Value::Float`; `as_int` would read it
            // as 0 and a partial store would zero the element's other bytes
            // (nio20 wave 2 L4, found reviewing the raw-store fallback).
            cratonvm_types::ArrayElementType::Float => match cur {
                Value::Float(f) => u64::from(f.to_bits()).to_le_bytes(),
                Value::Int(i) => u64::from(i as u32).to_le_bytes(),
                _ => 0u64.to_le_bytes(),
            },
            _ => ((cur.as_int().unwrap_or(0) as i64) as u64).to_le_bytes(),
        };
        while byte_in_elem < width && consumed < bytes.len() {
            elem_bytes[byte_in_elem] = bytes[consumed];
            byte_in_elem += 1;
            consumed += 1;
        }
        let raw = u64::from_le_bytes(elem_bytes);
        let new_val = match et {
            cratonvm_types::ArrayElementType::Boolean | cratonvm_types::ArrayElementType::Byte => {
                Value::Int(raw as u8 as i8 as i32)
            }
            cratonvm_types::ArrayElementType::Char => Value::Int(raw as u16 as i32),
            cratonvm_types::ArrayElementType::Short => Value::Int(raw as u16 as i16 as i32),
            cratonvm_types::ArrayElementType::Int => Value::Int(raw as u32 as i32),
            cratonvm_types::ArrayElementType::Float => Value::Float(f32::from_bits(raw as u32)),
            cratonvm_types::ArrayElementType::Long => Value::Long(raw as i64),
            // A `double[]` element is a `Value::Double`, NOT the `Value::Long`
            // this arm used to share with `long[]`. MEASURED 2026-09-20
            // (lane 4 wave 18, `L4TypedBufferSweep`, linux/x86_64):
            // `ByteBuffer.allocate(32).asDoubleBuffer()` filled with
            // {1.5, 2.5, 3.5, 4.5}, then `get(out, 1, 2)`, answered
            // `[0.0, 0.0, 0.0, 0.0]` where HotSpot answers
            // `[0.0, 2.5, 3.5, 0.0]` -- and only for `double`, because the
            // `float` arm below already built a `Value::Float` and the
            // integral widths are `Value::Int`/`Value::Long` anyway. The
            // store of a `Long` into a `double[]` does not land, so the
            // elements keep their zero: a SILENT WRONG ANSWER on every
            // `Unsafe.copyMemory` whose destination is a `double[]` --
            // `DoubleBuffer.get(double[],int,int)` above the
            // `JNI_COPY_TO_ARRAY_THRESHOLD`, `MemorySegment.toArray(JAVA_DOUBLE)`
            // and the FFM bulk paths among them. It was invisible while
            // `DoubleBuffer.get([DII)` was a native; wave 17 retired that row
            // and the probe found it on the first Linux run.
            cratonvm_types::ArrayElementType::Double => Value::Double(f64::from_bits(raw)),
            cratonvm_types::ArrayElementType::Reference => unreachable!(),
        };
        ctx.set_array_element(arr, elem_idx, new_val);
        byte_in_elem = 0;
        elem_idx += 1;
    }
    true
}

pub(crate) fn native_unsafe_copy_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, srcObj, srcOffset, destObj, destOffset, bytes]
    let src_obj = unsafe_obj(args, 1);
    let src_offset = unsafe_offset(args, 2);
    let dest_obj = unsafe_obj(args, 3);
    let dest_offset = unsafe_offset(args, 4);
    let bytes = match args.get(5) {
        Some(Value::Long(b)) => *b as usize,
        Some(Value::Int(b)) => *b as usize,
        _ => 0,
    };
    if bytes == 0 {
        return Ok(None);
    }
    if bytes > MAX_UNSAFE_COPY_SIZE {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: format!(
                "Unsafe.copyMemory size {} exceeds maximum of {} bytes",
                bytes, MAX_UNSAFE_COPY_SIZE
            ),
        }
        .into());
    }
    if let (Some(src), Some(dst)) = (src_obj, dest_obj) {
        // Primitive-array → primitive-array: `copyMemory` is a raw byte copy
        // (the JDK's NIO buffer-view classes — ByteBufferAsCharBuffer,
        // ICUBinary.getChars/getInts — rely on this to move trie data from a
        // `byte[]`-backed buffer into a `char[]`/`int[]`). Decompose each
        // source element to bytes and reassemble into the destination
        // element type, honoring per-element byte width and offsets.
        let src_is_array = ctx.heap_kind_of(src) == cratonvm_types::ObjectKind::Array
            && ctx.heap_element_type_of(src) != cratonvm_types::ArrayElementType::Reference;
        let dst_is_array = ctx.heap_kind_of(dst) == cratonvm_types::ObjectKind::Array
            && ctx.heap_element_type_of(dst) != cratonvm_types::ArrayElementType::Reference;
        if src_is_array || dst_is_array {
            // At least one side is a primitive array: the only correct copy is
            // the byte-exact array path, which is bounds-checked. If it cannot
            // complete (out-of-bounds, or one side is not an array) we must NOT
            // fall through to the unbounded slot loop — that would let an
            // attacker-controlled `bytes`/offset drive `set_field` past the
            // array's storage (finding M3). Throw IndexOutOfBoundsException
            // (HotSpot's `Unsafe.copyMemory` behaviour on a bad range).
            if src_is_array && dst_is_array {
                if let Some(buf) = unsafe_array_read_bytes(ctx, src, src_offset, bytes) {
                    if unsafe_array_write_bytes(ctx, dst, dest_offset, &buf) {
                        return Ok(None);
                    }
                }
            }
            return Err(
                cratonvm_types::error::RuntimeError::aioobe_index_only(dest_offset as i32).into(),
            );
        }
        // JIT round 20 wave 8c: under byte field offsets both sides are
        // field BYTES (a byte no primitive field holds reads 0 / is not
        // written).
        if ctx.unsafe_byte_field_offsets() {
            for i in 0..bytes {
                let b = unsafe_field_byte_at(&*ctx, src, src_offset.wrapping_add(i)).unwrap_or(0);
                let _ = unsafe_field_set_byte_at(ctx, dst, dest_offset.wrapping_add(i), b);
            }
            return Ok(None);
        }
        // Legacy slot-by-slot copy for object-field (non-array) targets only.
        // `bytes` is capped above by MAX_UNSAFE_COPY_SIZE.
        for i in 0..bytes {
            let val = ctx.get_field(src, src_offset + i);
            ctx.set_field(dst, dest_offset + i, val);
        }
    }
    Ok(None)
}

pub(crate) fn native_unsafe_set_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset, bytes, value]
    let obj = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let offset = match args.get(2) {
        Some(Value::Long(v)) => *v as usize,
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let bytes = match args.get(3) {
        Some(Value::Long(v)) => *v as usize,
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let value = match args.get(4) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };
    if bytes == 0 {
        return Ok(None);
    }
    if bytes > MAX_UNSAFE_COPY_SIZE {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: format!(
                "Unsafe.setMemory size {} exceeds maximum of {} bytes",
                bytes, MAX_UNSAFE_COPY_SIZE
            ),
        }
        .into());
    }
    if let Some(obj_ref) = obj {
        // Primitive-array target: fill `bytes` raw bytes with `value`. This is
        // the only correct path for an array and it is bounds-checked. If it
        // cannot complete (out-of-bounds) we must NOT fall through to the
        // unbounded slot loop — doing so would let attacker-controlled
        // `bytes`/`offset` drive `set_field` past the array storage, and for
        // non-array objects the loop ran unconditionally (finding M3).
        if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array
            && ctx.heap_element_type_of(obj_ref) != cratonvm_types::ArrayElementType::Reference
        {
            let fill = vec![value; bytes];
            if unsafe_array_write_bytes(ctx, obj_ref, offset, &fill) {
                return Ok(None);
            }
            return Err(
                cratonvm_types::error::RuntimeError::aioobe_index_only(offset as i32).into(),
            );
        }
        // JIT round 20 wave 8c: under byte field offsets `bytes` counts the
        // fields' BYTES.
        if ctx.unsafe_byte_field_offsets() {
            if unsafe_field_fill_bytes(ctx, obj_ref, offset, bytes, value) {
                return Ok(None);
            }
            return Err(
                cratonvm_types::error::RuntimeError::aioobe_index_only(offset as i32).into(),
            );
        }
        // Object-field (non-array) target: bounded slot fill. `bytes` is capped
        // above by MAX_UNSAFE_COPY_SIZE.
        let fill_value = Value::Int(value as i32);
        for i in 0..bytes {
            ctx.set_field(obj_ref, offset + i, fill_value);
        }
    }
    Ok(None)
}

/// `Unsafe.allocateMemory(long)` — delegates to the arena-backed
/// [`unsafe_natives::native_unsafe_allocate_memory_consolidated`].
///
/// This used to allocate a managed `Object[]` and return its HEAP ADDRESS as
/// the "off-heap" pointer. The registration was always overwritten by
/// `register_consolidated_off_heap_store` (both boot paths call it later), but
/// the body was one registration-order change away from putting an unminted
/// heap address into a Java `long` — a value the operand-stack root scan would
/// root and the post-GC remap would never rewrite
/// (`docs/internal/fixed-bugs/interpreter-L4-operand-stack-roots-unminted-primitive-longs-FIXED-20260925.md`).
/// The heap block was also unreadable by the raw get/put natives, which only
/// address the arena. Delegating makes every registration of this triple mean
/// the same thing.
pub(crate) fn native_unsafe_allocate_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unsafe_natives::native_unsafe_allocate_memory_consolidated(ctx, args)
}

/// `Unsafe.freeMemory(long)` / `freeMemory0(long)` — release an off-heap block.
///
/// This was `native_noop_with_this`, i.e. every block handed out by
/// `allocateMemory` stayed live for the lifetime of the process. All off-heap
/// addressing in this VM is consolidated onto the arena store (see the V5
/// SECURITY FIX note in `unsafe_natives.rs`): `unsafe_arena_free` removes the
/// whole `[base, base+len)` entry from the store's `BTreeMap`, and that map IS
/// the bookkeeping table — dropping the entry both releases the bytes and
/// makes every later access to the address fail the liveness check, which is
/// what turns a use-after-free into an `IllegalArgumentException` instead of a
/// silent read of recycled memory.
///
/// `freeMemory(0)` is a no-op by contract (`Unsafe` mirrors C `free(NULL)`),
/// and an address the arena never handed out — e.g. a real
/// `ByteBuffer.allocateDirect` pointer, which does not carry the arena tag —
/// is left alone rather than reported as an error, matching the consolidated
/// `unsafe_natives::native_unsafe_free_memory` handler.
pub(crate) fn native_unsafe_free_memory_ext(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] is the `Unsafe` receiver; args[1] is the address.
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        Some(Value::Int(a)) => *a as i64,
        _ => return Ok(None),
    };
    if addr == 0 {
        return Ok(None);
    }
    unsafe_arena_free(addr);
    Ok(None)
}

/// `Unsafe.monitorEnter(Object)` — acquire the argument's monitor.
///
/// This was `native_noop_with_this`, which meant a caller that locks through
/// `Unsafe` rather than the `monitorenter` bytecode got NO mutual exclusion:
/// every thread "acquired" the monitor simultaneously and the critical section
/// ran concurrently. CratonVM already has a working monitor implementation —
/// `NativeThreadAccess::monitor_enter`/`monitor_exit`, the same pair that backs
/// `java/lang/Object.wait`/`notify`/`notifyAll` — so route to it.
///
/// A null argument is an NPE in HotSpot; `obj_arg` produces exactly that.
/// Plain `monitor_enter` (not `monitor_enter_gc_safe`) is correct here: the
/// receiver is not touched after the acquire, so a relocation during a blocked
/// enter cannot leave a stale reference behind.
pub(crate) fn native_unsafe_monitor_enter(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] is the `Unsafe` receiver; args[1] is the object to lock.
    let obj = obj_arg(args, 1)?;
    ctx.monitor_enter(obj);
    Ok(None)
}

/// `Unsafe.monitorExit(Object)` — release the argument's monitor.
/// Counterpart of [`native_unsafe_monitor_enter`]; see its doc for why the
/// previous no-op was a correctness bug.
pub(crate) fn native_unsafe_monitor_exit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = obj_arg(args, 1)?;
    ctx.monitor_exit(obj);
    Ok(None)
}

/// `Unsafe.reallocateMemory(long, long)` — delegates to the arena-backed
/// [`unsafe_natives::native_unsafe_reallocate_memory_consolidated`], for the
/// reason given on [`native_unsafe_allocate_memory`] (this body also returned
/// a heap `Object[]` address, and dropped the old block's contents).
pub(crate) fn native_unsafe_allocate_memory_realloc(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unsafe_natives::native_unsafe_reallocate_memory_consolidated(ctx, args)
}

// ===========================================================================
// WP1.2 — Process-local arena / cleaner registries used by unsafe_natives.
//
// The arena/cleaner state used to live over in `vm/src/runtime/unsafe_helpers.rs`
// but the crate graph is `vm -> native-builtins`, so putting the store
// there would create a cycle. The authoritative store now lives in
// this crate (next to every other Unsafe native); `unsafe_helpers` in
// vm now provides a VM-internal facade over the same singletons via
// the `LocalUnsafeArena` trait in native-api. That trait gives
// VM-side subsystems (GC, JIT arena cleanup on shutdown) access
// without forcing them to take a dependency on this crate.
// ===========================================================================

mod unsafe_arena {
    use parking_lot::{Mutex, RwLock};
    use std::collections::BTreeMap;

    /// R1 (silent-corruption fix): reserved high tag bit OR-ed into every
    /// arena handle. Real OS pointers on every platform CratonVM targets live
    /// in the low address space — Windows x64 user mode is capped at 2^47-1
    /// (128 TiB) and even Linux 5-level paging tops user space at 2^56-1 — so
    /// bit 62 is NEVER set on a real pointer (e.g. a `ByteBuffer.allocateDirect`
    /// address from `dbb_allocate`). Setting it on every handle makes arena
    /// handles *provably disjoint* from real pointers, so the range-membership
    /// scan in `locate`/`contains` can never mis-classify a real pointer that
    /// happens to fall numerically inside a live block's `[base, base+len)` as
    /// an arena handle and silently corrupt it.
    ///
    /// The tag is part of the address value end-to-end — it is NEVER stripped.
    /// Handles flow opaquely through Java as `long` (`DirectByteBuffer.address()`,
    /// `Unsafe.get/put/copy/free`) and come back to `locate` unchanged, so every
    /// consumer round-trips the tagged value. We deliberately avoid bit 63 so
    /// handles stay positive `i64`.
    pub(super) const ARENA_TAG: i64 = 1 << 62; // 0x4000_0000_0000_0000
    /// nio21 wave 1 L3 (the JIT-readable arena): bit 61 marks a LEGACY handle,
    /// one bumped out of the byte-granular handle space below. A handle with
    /// bit 62 set and bit 61 clear is a SLOTTED handle (see [`JitSlot`]).
    /// Nothing outside this module reads bit 61: every other consumer tests
    /// bit 62 only, and both kinds round-trip as opaque `long`s.
    const LEGACY_BIT: i64 = 1 << 61;
    /// First legacy handle handed out: the tag and the legacy bit OR-ed onto
    /// the historical 2^36 base.
    const ARENA_BASE: i64 = ARENA_TAG | LEGACY_BIT | 0x10_0000_0000;

    // -----------------------------------------------------------------------
    // The JIT-readable arena (nio21 wave 1 L3; page
    // `docs/internal/fixed-bugs/r20-directbb-every-access-is-two-calls-and-an-arena-lookup-CLOSED-20261012.md`)
    // -----------------------------------------------------------------------
    //
    // A block smaller than `2^JIT_SLOT_SHIFT` bytes gets a SLOTTED handle
    // `ARENA_TAG | seq << JIT_SLOT_SHIFT`, where `seq` is a process-monotonic
    // 31-bit sequence number (bits 30..60), and its `(ptr, len)` is published in
    // `table[seq & (JIT_SLOTS - 1)]` under the key = the handle itself. The
    // optimizing tier's scoped fold (`jit/src/ir_lower.rs`
    // `Lowerer::emit_scoped_arena_resolve`) then resolves any address inside
    // the block with no lock and no call:
    //
    //   slot = table + ((a >> 30) & mask) * 32
    //   key  = a & !(2^30 - 1)
    //   [slot].key == key  ;  (a & (2^30-1)) + width <= [slot].len
    //   real = [slot].ptr + (a & (2^30-1))  ;  [slot].key == key again
    //
    // # Why a key that matches twice makes `(ptr, len)` the block's own
    //
    // * A key is published at most ONCE in the life of the process: `seq` never
    //   repeats (it is never reset, and an exhausted sequence falls back to
    //   legacy handles), so after a slot's key is cleared that key never
    //   reappears in any slot.
    // * The writer stores `ptr`, `len`, then `key` (release); a reader loads
    //   `key`, `len`, `ptr`, `key` (x86-64 loads are not reordered with older
    //   loads, and stores are seen in one total order). If both key loads see
    //   K, no clear of K had become visible before the second one, so neither
    //   had the next occupant's `ptr` / `len` (stored after that clear); and
    //   the first load seeing K means K's own `ptr` / `len` were visible.
    // * `free` and `reallocate` clear the key BEFORE they touch the block, so a
    //   use-after-free or use-after-realloc that is ordered after the free (the
    //   same thread, or a happens-before edge) misses and leaves for the
    //   interpreter, whose slow path keeps answering "no such block".
    //
    // # What it does not cover (the same as HotSpot)
    //
    // A `freeMemory` on another thread RACING a compiled access to the same
    // block (no happens-before between them) may let the access touch the
    // freed buffer. That is a data race on raw memory that the Java program
    // owns, and HotSpot's `free(3)` gives no more. Every path the fold serves
    // has no such race by construction: a `DirectByteBuffer` frees its block
    // through its `Cleaner` only once it is unreachable, and the compiled
    // access holds it (the `reachabilityFence(this)` keep-alive node, and no
    // safepoint between the lookup and the access); a confined session closes
    // only on its owner thread, which is the accessing thread; the never-closing
    // sessions never close; a SHARED session is not admitted by the fold (it
    // keeps its deopt), so a shared close never frees memory under a compiled
    // access.
    //
    // A block that is reallocated in place loses its slot for good (the key
    // cannot be re-published), and keeps working through the locked slow path.
    // `CRATONVM_UNSAFE_ARENA_JIT_TABLE=0` (read once, when the store is built)
    // hands out legacy handles only and publishes nothing.

    /// Low bits of a slotted handle that address inside its block.
    pub(super) const JIT_SLOT_SHIFT: u32 = 30;
    /// Bits of `seq` that index the table.
    pub(super) const JIT_INDEX_BITS: u32 = 15;
    /// `log2(size_of::<JitSlot>())`.
    pub(super) const JIT_ENTRY_SHIFT: u32 = 5;
    const JIT_SLOTS: usize = 1 << JIT_INDEX_BITS;
    /// `seq` is 31 bits (handle bits 30..60); bit 61 stays the legacy bit.
    const JIT_SEQ_LIMIT: u64 = 1 << 31;
    /// A slotted block must be strictly shorter than its handle window, so
    /// every offset inside it (and one past its end) keeps the window's key.
    const JIT_MAX_BLOCK: usize = (1 << JIT_SLOT_SHIFT) - 16;
    /// How many occupied table entries a claim skips before it settles for a
    /// legacy handle.
    const JIT_CLAIM_TRIES: usize = 64;
    /// A table entry claimed but not yet published. Never a handle's key: a
    /// key has its low 30 bits clear.
    const JIT_CLAIMED: i64 = 1;

    /// One entry of the JIT-readable table. `#[repr(C)]`, 32 bytes: the
    /// optimizing tier bakes the field offsets (`ir_lower::SCOPED_ARENA_*`;
    /// the VM's `jit_well_known_class_id` arm asserts they agree).
    #[repr(C, align(32))]
    #[derive(Default)]
    pub(super) struct JitSlot {
        /// The slotted handle of the block published here, `0` for a free
        /// entry, [`JIT_CLAIMED`] while an allocation is publishing it.
        key: std::sync::atomic::AtomicI64,
        /// The block's length in bytes.
        len: std::sync::atomic::AtomicU64,
        /// The real address of the block's first byte.
        ptr: std::sync::atomic::AtomicUsize,
    }

    pub(super) const JIT_SLOT_KEY_OFFSET: usize = std::mem::offset_of!(JitSlot, key);
    pub(super) const JIT_SLOT_LEN_OFFSET: usize = std::mem::offset_of!(JitSlot, len);
    pub(super) const JIT_SLOT_PTR_OFFSET: usize = std::mem::offset_of!(JitSlot, ptr);
    const _: () = assert!(std::mem::size_of::<JitSlot>() == 1 << JIT_ENTRY_SHIFT);
    const _: () = assert!(JIT_SLOT_SHIFT as u64 + 31 == 61);

    /// The two handle cursors, under the one `next_addr` mutex.
    struct HandleCursor {
        /// Next legacy handle.
        legacy: i64,
        /// Next slotted `seq`; [`JIT_SEQ_LIMIT`] once exhausted.
        seq: u64,
    }

    struct Arena {
        bytes: Vec<u8>,
        /// Bytes of HANDLE space this block owns, which is what `try_allocate`
        /// bumped `next_addr` by -- not what the block currently holds.
        ///
        /// `locate` resolves an address to the greatest base at or below it, so
        /// a block whose `bytes` grow past this would answer for handles
        /// already issued to the blocks above it: two live blocks aliasing one
        /// address range, with every access to the later one silently
        /// redirected into the earlier. `reallocate` keeps
        /// `bytes.len() <= reserved` and relocates rather than break it.
        reserved: usize,
    }

    /// Handle space a block of `size` bytes owns. `next_addr` bumps by this,
    /// and [`Arena::reserved`] records it.
    fn reserved_for(size: usize) -> usize {
        (size.max(1) + 15) & !15
    }

    pub(super) struct ArenaStore {
        inner: RwLock<BTreeMap<i64, Arena>>,
        next_addr: Mutex<HandleCursor>,
        /// The JIT-readable table (see the section comment above
        /// [`JIT_SLOT_SHIFT`]); empty when `CRATONVM_UNSAFE_ARENA_JIT_TABLE=0`.
        /// Allocated once and never resized, so its address is fixed for the
        /// life of the process (the store is never dropped).
        jit: Box<[JitSlot]>,
        /// Blocks whose REAL pointer has been handed to native code, and how
        /// many times. See [`ArenaStore::real_ptr`].
        ///
        /// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Every acquisition
        /// is a single statement over a `BTreeMap<i64, u64>` with no
        /// `NativeContext` in scope.
        ///
        /// It is safely `Scratch` only BECAUSE the enclosing lock is
        /// unordered: `real_ptr` holds `inner.write()` across this
        /// acquisition. Same caveat the JFR tables carry — if `inner` is ever
        /// given a level it must be a HIGHER one than this, never an equal.
        translated: cratonvm_types::lock_order::OrderedPlMutex<BTreeMap<i64, u64>>,
        /// The backing buffer a `reallocate` displaced out from under an
        /// OUTSTANDING real pointer, kept alive so that the pointer keeps
        /// naming memory this process owns. See [`ArenaStore::reallocate`].
        ///
        /// LOCK LEVEL: `Scratch`, for the same reason `translated` is. Unlike
        /// `translated` it is acquired with NO other arena lock held, so the
        /// caveat there about the enclosing `inner.write()` does not apply.
        retired: cratonvm_types::lock_order::OrderedPlMutex<BTreeMap<i64, Vec<u8>>>,
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Real pointers handed to native code — see [`ArenaStore::real_ptr`].
    pub(super) static TRANSLATIONS: AtomicU64 = AtomicU64::new(0);
    /// Reallocations of a block whose real pointer was already handed out.
    ///
    /// **`Vec::resize` may MOVE the buffer**, so every pointer handed out before
    /// this call is now dangling and points at memory the process allocator has
    /// taken back. Nonzero here means native code is holding at least one such
    /// pointer.
    pub(super) static STALE_ON_REALLOC: AtomicU64 = AtomicU64::new(0);
    /// Frees of a block whose real pointer was already handed out. Same hazard:
    /// the `Vec` is dropped and its memory returned to the allocator.
    pub(super) static STALE_ON_FREE: AtomicU64 = AtomicU64::new(0);
    /// Displaced buffers that [`ArenaStore::reallocate`] RETAINED rather than
    /// dropped, because a real pointer into them was outstanding. This is the
    /// count of times the dangling-pointer window above was closed.
    pub(super) static RETAINED_ON_REALLOC: AtomicU64 = AtomicU64::new(0);
    /// Translations refused by `unsafe_arena_real_ptr_bounded` because the
    /// block could not cover the length being advertised with the pointer.
    pub(super) static SHORT_TRANSLATIONS: AtomicU64 = AtomicU64::new(0);

    impl ArenaStore {
        fn new() -> Self {
            let jit: Box<[JitSlot]> = if cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_UNSAFE_ARENA_JIT_TABLE",
            ) {
                (0..JIT_SLOTS).map(|_| JitSlot::default()).collect()
            } else {
                Vec::new().into_boxed_slice()
            };
            let seq = if jit.is_empty() { JIT_SEQ_LIMIT } else { 1 };
            Self {
                inner: RwLock::new(BTreeMap::new()),
                next_addr: Mutex::new(HandleCursor {
                    legacy: ARENA_BASE,
                    seq,
                }),
                jit,
                translated: cratonvm_types::lock_order::OrderedPlMutex::new(
                    BTreeMap::new(),
                    cratonvm_types::lock_order::LockLevel::Scratch,
                ),
                retired: cratonvm_types::lock_order::OrderedPlMutex::new(
                    BTreeMap::new(),
                    cratonvm_types::lock_order::LockLevel::Scratch,
                ),
            }
        }

        /// `None` when the backing buffer could not be allocated.
        ///
        /// `vec![0u8; size]` is INFALLIBLE: on failure Rust runs the
        /// allocation-error hook, which aborts the process. That is what
        /// `Unsafe.allocateMemory(Long.MAX_VALUE)` did -- SIGABRT with
        /// `memory allocation of 9223372036854775807 bytes failed`, taking the
        /// whole VM down where HotSpot throws. A Java caller must never be
        /// able to abort the runtime by passing a large number to a method
        /// whose contract is "returns 0 / throws OutOfMemoryError".
        pub(super) fn try_allocate(&self, size: usize) -> Option<i64> {
            let mut bytes: Vec<u8> = Vec::new();
            bytes.try_reserve_exact(size).ok()?;
            bytes.resize(size, 0u8);
            let reserved = reserved_for(size);
            let (addr, slotted) = self.claim_handle(size, reserved);
            // The heap buffer does not move when the `Vec` moves into the map.
            let (ptr, len) = (bytes.as_ptr() as usize, bytes.len());
            self.inner.write().insert(addr, Arena { bytes, reserved });
            if slotted {
                self.jit_publish(addr, ptr, len);
            }
            Some(addr)
        }

        /// Claim handle space for a block. Never reuses an address (neither
        /// cursor ever moves back): a freed block's handles stay dead so a
        /// use-after-free keeps resolving to nothing instead of to somebody
        /// else's bytes.
        ///
        /// A fresh handle for a block of `size` bytes owning `reserved` bytes of
        /// handle space: SLOTTED (`true`, its table entry claimed for it) when
        /// the table is on, the block fits a window, `seq` is not exhausted and
        /// one of the next [`JIT_CLAIM_TRIES`] entries is free; LEGACY otherwise.
        ///
        /// Takes no lock but `next_addr`, so it is safe to call while `inner`
        /// is held.
        fn claim_handle(&self, size: usize, reserved: usize) -> (i64, bool) {
            let mut c = self.next_addr.lock();
            if !self.jit.is_empty() && size > 0 && size < JIT_MAX_BLOCK {
                for _ in 0..JIT_CLAIM_TRIES {
                    let seq = c.seq;
                    if seq >= JIT_SEQ_LIMIT {
                        break;
                    }
                    c.seq = seq + 1;
                    // Cast: `seq < 2^31`, so the index is in range.
                    let slot = &self.jit[(seq as usize) & (JIT_SLOTS - 1)];
                    // Only a `0` entry is claimable, and only under this lock;
                    // `jit_unpublish` is the one writer of `0`.
                    if slot.key.load(Ordering::Acquire) == 0 {
                        slot.key.store(JIT_CLAIMED, Ordering::Relaxed);
                        // Cast: `seq < 2^31`, so `seq << 30 < 2^61`.
                        return (ARENA_TAG | ((seq as i64) << JIT_SLOT_SHIFT), true);
                    }
                }
            }
            let a = c.legacy;
            c.legacy = c.legacy.saturating_add(reserved as i64);
            (a, false)
        }

        /// The table entry a SLOTTED handle `addr` indexes, or `None` for a
        /// legacy or untagged address (or the table off).
        fn jit_slot_of(&self, addr: i64) -> Option<&JitSlot> {
            if self.jit.is_empty() || addr & ARENA_TAG == 0 || addr & LEGACY_BIT != 0 {
                return None;
            }
            // Cast: a positive i64 shifted right; masked to the index bits.
            let index = ((addr >> JIT_SLOT_SHIFT) as usize) & (JIT_SLOTS - 1);
            self.jit.get(index)
        }

        /// Publish the block whose (slotted) handle is `addr`: `ptr` / `len`
        /// first, the key last, with release order (see the section comment).
        /// Its entry was claimed for it by [`Self::claim_handle`].
        fn jit_publish(&self, addr: i64, ptr: usize, len: usize) {
            let Some(slot) = self.jit_slot_of(addr) else {
                return;
            };
            slot.ptr.store(ptr, Ordering::Relaxed);
            slot.len.store(len as u64, Ordering::Relaxed);
            slot.key.store(addr, Ordering::Release);
        }

        /// Withdraw the block whose handle is `addr` from the table, for good:
        /// its key will never be published again. A no-op unless the entry is
        /// that block's. Called BEFORE the block is freed or resized.
        fn jit_unpublish(&self, addr: i64) {
            if let Some(slot) = self.jit_slot_of(addr) {
                let _ = slot
                    .key
                    .compare_exchange(addr, 0, Ordering::AcqRel, Ordering::Relaxed);
            }
        }

        /// The table's address, `0` when it is off. Fixed for the process.
        pub(super) fn jit_table_addr(&self) -> usize {
            if self.jit.is_empty() {
                0
            } else {
                self.jit.as_ptr() as usize
            }
        }

        /// Is the block whose handle is `addr` published in the table now?
        #[cfg(test)]
        pub(super) fn jit_published(&self, addr: i64) -> bool {
            self.jit_slot_of(addr)
                .is_some_and(|s| s.key.load(Ordering::Acquire) == addr)
        }

        /// What the optimizing tier's inline resolve answers for `addr` and an
        /// access of `width` bytes: the real address, or `None` (the fold's
        /// deopt). The same steps in the same order, for the tests.
        #[cfg(test)]
        pub(super) fn jit_resolve(&self, addr: i64, width: u64) -> Option<usize> {
            let slot = self.jit_slot_of(addr)?;
            let key = addr & !((1i64 << JIT_SLOT_SHIFT) - 1);
            if slot.key.load(Ordering::Acquire) != key {
                return None;
            }
            // Cast: masked to 30 bits.
            let off = (addr & ((1i64 << JIT_SLOT_SHIFT) - 1)) as u64;
            if off + width > slot.len.load(Ordering::Relaxed) {
                return None;
            }
            let real = slot.ptr.load(Ordering::Relaxed) + off as usize;
            if slot.key.load(Ordering::Acquire) != key {
                return None;
            }
            Some(real)
        }

        pub(super) fn allocate(&self, size: usize) -> i64 {
            self.try_allocate(size).unwrap_or(0)
        }

        /// The live block holding `addr`, for the length of ONE downcall
        /// (`panama_libffi::arena_call_address`): the real pointer for `addr`,
        /// the block's real base, its length and its handle base. `None` for an
        /// untagged address, a freed handle, or a block that cannot cover `want`
        /// bytes from `addr` (`want == 0` may name one past the end). Records no
        /// translation: the caller holds the block's session for the call, so
        /// the block cannot be freed before the C function returns (a close
        /// meets the acquire), and reallocation never touches an arena block.
        pub(super) fn peek(&self, addr: i64, want: usize) -> Option<(*mut u8, usize, usize, i64)> {
            if addr & ARENA_TAG == 0 {
                return None;
            }
            let inner = self.inner.read();
            let (&base, arena) = inner.range(..=addr).next_back()?;
            let offset = usize::try_from(addr - base).ok()?;
            let len = arena.bytes.len();
            if offset > len || (offset == len && want > 0) || len - offset < want {
                return None;
            }
            let real = arena.bytes.as_ptr() as *mut u8;
            // SAFETY: `offset <= len`, inside (or one past the end of) the
            // block's own allocation.
            Some((unsafe { real.add(offset) }, real as usize, len, base))
        }

        /// End ONE outstanding translation of the block holding `addr` (the pointer a
        /// native call took has been given up: the call returned). The counterpart of
        /// `real_ptr`'s record, so a short-lived pointer does not leave the block
        /// "translated" for `free` to report. A no-op for an untagged or unknown address.
        pub(super) fn release_ptr(&self, addr: i64) {
            if addr & ARENA_TAG == 0 {
                return;
            }
            let base = {
                let inner = self.inner.read();
                match Self::locate(&inner, addr) {
                    Some((base, _)) => base,
                    None => return,
                }
            };
            let mut translated = self.translated.lock();
            if let Some(n) = translated.get_mut(&base) {
                if *n > 1 {
                    *n -= 1;
                } else {
                    translated.remove(&base);
                }
            }
        }

        #[cfg(test)]
        pub(super) fn block_is_translated(&self, addr: i64) -> bool {
            self.translated.lock().contains_key(&addr)
        }

        #[cfg(test)]
        pub(super) fn block_is_retired(&self, addr: i64) -> bool {
            self.retired.lock().contains_key(&addr)
        }

        /// `None` when the resize could not be allocated -- see
        /// [`Self::try_allocate`]. The block is left UNTOUCHED on failure,
        /// which is what `realloc(3)` guarantees and what the JDK's
        /// `reallocateMemory` contract relies on when it throws.
        ///
        /// `Some` carries the handle the block now lives at, which is NOT
        /// necessarily the one passed in -- see [`Self::reallocate`].
        pub(super) fn try_reallocate(&self, addr: i64, new_size: usize) -> Option<i64> {
            match self.reallocate(addr, new_size) {
                0 => None,
                a => Some(a),
            }
        }

        /// Resize a block: 0 if the new size could not be allocated, otherwise
        /// the handle the block now lives at, WHICH MAY DIFFER FROM `addr`.
        ///
        /// # Growing past the reserved handle space RELOCATES
        ///
        /// `try_allocate` bumps `next_addr` by the rounded request, so a block
        /// owns exactly that much handle space, and `locate` resolves an
        /// address to the greatest base at or below it. Growing `bytes` past
        /// that made the block answer for every handle issued after it: two
        /// live blocks aliasing one range, with reads and writes through the
        /// LATER handle silently landing in the earlier block. A 64-byte block
        /// grown to 4096 swallowed its successor whole.
        ///
        /// So a grow that will not fit is served from a fresh handle and the
        /// old one dies, which is what `realloc(3)` does and what
        /// `Unsafe.reallocateMemory` already documents -- its caller must use
        /// the returned address, and `native_unsafe_reallocate_memory_consolidated`
        /// passes this straight back to Java.
        ///
        /// # Keeping any buffer a real pointer still names
        ///
        /// `Vec::resize` reallocates when it must grow past its capacity, and a
        /// reallocation MOVES the bytes; so does relocating. Every real pointer
        /// [`Self::real_ptr`] handed out before that point would then name
        /// memory the process allocator has taken back -- an unbounded
        /// use-after-free from native code, which is the shape
        /// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` spent
        /// seven passes hunting.
        ///
        /// So a block whose pointer is OUTSTANDING does not lose its old
        /// buffer: the displaced `Vec` moves to `retired`, keyed by whatever
        /// handle the block ends up at, and stays owned by this process until
        /// that handle is freed. The stale pointer then reads stale-but-mapped
        /// bytes instead of corrupting whatever the allocator handed out next.
        /// That is a downgrade, not a cure -- the native is still using an
        /// address the Java side has moved on from -- but it bounds the blast
        /// radius to the block itself, and `STALE_ON_REALLOC` reports every
        /// occurrence.
        ///
        /// Retention is at most ONE buffer per handle: a second displacement
        /// drops the first, and `free` drops whatever is left. A resize that
        /// does NOT move the buffer retains nothing and reports nothing, which
        /// is why `STALE_ON_REALLOC` counts moves rather than resizes.
        pub(super) fn reallocate(&self, addr: i64, new_size: usize) -> i64 {
            // nio21 wave 1 L3: out of the JIT-readable table BEFORE anything
            // moves or shrinks, for good (a key is never re-published), so a
            // compiled access ordered after this resize takes the slow path.
            // Done even when the resize then fails: that only costs the block
            // its inline route.
            self.jit_unpublish(addr);
            // BEFORE anything moves, which is the only moment this is knowable.
            let outstanding = self.translated.lock().get(&addr).copied();
            let mut inner = self.inner.write();
            let old = inner.remove(&addr);
            let reserved = old.as_ref().map(|a| a.reserved).unwrap_or(0);
            let mut bytes = old.map(|a| a.bytes).unwrap_or_default();

            // A resize the block's own handle space cannot hold, or a resize of
            // a handle that names no live block at all (`reallocate` must not
            // conjure a block at an arbitrary address -- that is the aliasing
            // above by another route). Both are served from a fresh handle.
            if new_size > reserved {
                let mut fresh: Vec<u8> = Vec::new();
                if fresh.try_reserve_exact(new_size).is_err() {
                    // `realloc(3)`'s contract on failure: the block is left
                    // exactly as it was.
                    if reserved != 0 {
                        inner.insert(addr, Arena { bytes, reserved });
                    }
                    return 0;
                }
                fresh.extend_from_slice(&bytes[..new_size.min(bytes.len())]);
                fresh.resize(new_size, 0u8);
                let new_reserved = reserved_for(new_size);
                let (new_addr, slotted) = self.claim_handle(new_size, new_reserved);
                let (fresh_ptr, fresh_len) = (fresh.as_ptr() as usize, fresh.len());
                inner.insert(
                    new_addr,
                    Arena {
                        bytes: fresh,
                        reserved: new_reserved,
                    },
                );
                drop(inner);
                if slotted {
                    self.jit_publish(new_addr, fresh_ptr, fresh_len);
                }
                self.displace(addr, new_addr, bytes, outstanding, new_size);
                return new_addr;
            }

            // In place: the block keeps its handle. It can still MOVE in
            // memory, because `reserved` bounds the handle space and
            // `capacity` bounds the buffer, and those are different numbers --
            // a 60-byte block reserves 64 and has capacity 60.
            if outstanding.is_some() && new_size > bytes.capacity() {
                let mut fresh: Vec<u8> = Vec::new();
                if fresh.try_reserve_exact(new_size).is_err() {
                    // Fail the resize rather than fall through and let
                    // `Vec::resize` move the buffer with no way to retain the
                    // old one. `realloc(3)` leaving the block untouched is a
                    // contract the caller already handles; a silent dangle is
                    // not.
                    inner.insert(addr, Arena { bytes, reserved });
                    return 0;
                }
                fresh.extend_from_slice(&bytes[..new_size.min(bytes.len())]);
                fresh.resize(new_size, 0u8);
                inner.insert(
                    addr,
                    Arena {
                        bytes: fresh,
                        reserved,
                    },
                );
                drop(inner);
                self.displace(addr, addr, bytes, outstanding, new_size);
                return addr;
            }
            if new_size > bytes.len() && bytes.try_reserve_exact(new_size - bytes.len()).is_err() {
                inner.insert(addr, Arena { bytes, reserved });
                return 0;
            }
            bytes.resize(new_size, 0u8);
            inner.insert(addr, Arena { bytes, reserved });
            addr
        }

        /// Move the translation bookkeeping from `from` to `to` and retain the
        /// buffer `displaced` if a real pointer into it is outstanding.
        ///
        /// The new block is deliberately NOT marked translated: nobody has been
        /// given a pointer into it. The retained buffer is keyed by the block's
        /// new handle so that freeing the block frees it too.
        ///
        /// Takes no arena lock but `translated`/`retired`, so callers must have
        /// released `inner`.
        fn displace(
            &self,
            from: i64,
            to: i64,
            displaced: Vec<u8>,
            outstanding: Option<u64>,
            new_size: usize,
        ) {
            let mut translated = self.translated.lock();
            translated.remove(&from);
            drop(translated);
            let mut retired = self.retired.lock();
            let carried = retired.remove(&from);
            match outstanding {
                Some(n) => {
                    STALE_ON_REALLOC.fetch_add(1, Ordering::Relaxed);
                    RETAINED_ON_REALLOC.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        target: "cratonvm::unsafe_arena",
                        handle = format!("{from:#x}"),
                        now_at = format!("{to:#x}"),
                        translations = n,
                        old_size = displaced.len(),
                        new_size,
                        "Unsafe-arena block MOVED while native code holds a real \
                         pointer into it -- the displaced buffer is RETAINED so that \
                         pointer keeps naming memory this process owns, and is \
                         dropped when the handle is freed"
                    );
                    retired.insert(to, displaced);
                }
                None => {
                    // Nothing is holding the displaced buffer, so it dies here.
                    // Any buffer retained by an EARLIER displacement follows the
                    // block to its new handle rather than being orphaned.
                    if let Some(prev) = carried {
                        retired.insert(to, prev);
                    }
                }
            }
        }

        pub(super) fn free(&self, addr: i64) {
            if let Some(n) = self.translated.lock().remove(&addr) {
                STALE_ON_FREE.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    target: "cratonvm::unsafe_arena",
                    handle = format!("{addr:#x}"),
                    translations = n,
                    "Unsafe-arena block is being FREED while native code holds a real pointer into it -- the `Vec` is dropped and its memory returned to the process allocator"
                );
            }
            // nio21 wave 1 L3: out of the JIT-readable table BEFORE the buffer
            // dies, so a compiled access ordered after this free misses.
            self.jit_unpublish(addr);
            self.inner.write().remove(&addr);
            // Whatever a resize retained for this handle dies with it. Freeing
            // is the point at which the JNI/`Unsafe` contract says the pointer
            // is dead on any VM, so there is nothing to preserve past it --
            // and retaining past `free` would leak on an
            // allocate/translate/free loop.
            self.retired.lock().remove(&addr);
        }

        pub(super) fn get_byte(&self, addr: i64) -> u8 {
            self.read::<1>(addr).map(|b| b[0]).unwrap_or(0)
        }

        // audit-round5 fix #3 (HIGH): bounds-checked getters used by the
        // get-side natives to throw IAE on out-of-arena addresses
        // (mirror of the put-side IAE behavior fixed in round-4 wave-1).
        pub(super) fn try_get_byte(&self, addr: i64) -> Option<u8> {
            self.read::<1>(addr).map(|b| b[0])
        }

        pub(super) fn put_byte(&self, addr: i64, v: u8) -> bool {
            self.write::<1>(addr, &[v])
        }

        pub(super) fn get_short(&self, addr: i64) -> i16 {
            i16::from_le_bytes(self.read::<2>(addr).unwrap_or([0; 2]))
        }

        pub(super) fn try_get_short(&self, addr: i64) -> Option<i16> {
            self.read::<2>(addr).map(i16::from_le_bytes)
        }

        pub(super) fn put_short(&self, addr: i64, v: i16) -> bool {
            self.write::<2>(addr, &v.to_le_bytes())
        }

        pub(super) fn get_int(&self, addr: i64) -> i32 {
            i32::from_le_bytes(self.read::<4>(addr).unwrap_or([0; 4]))
        }

        pub(super) fn try_get_int(&self, addr: i64) -> Option<i32> {
            self.read::<4>(addr).map(i32::from_le_bytes)
        }

        pub(super) fn put_int(&self, addr: i64, v: i32) -> bool {
            self.write::<4>(addr, &v.to_le_bytes())
        }

        pub(super) fn get_long(&self, addr: i64) -> i64 {
            i64::from_le_bytes(self.read::<8>(addr).unwrap_or([0; 8]))
        }

        pub(super) fn try_get_long(&self, addr: i64) -> Option<i64> {
            self.read::<8>(addr).map(i64::from_le_bytes)
        }

        pub(super) fn put_long(&self, addr: i64, v: i64) -> bool {
            self.write::<8>(addr, &v.to_le_bytes())
        }

        // audit-2026-05-16: find the arena whose `[base, base+size)`
        // contains `addr`. The previous impl looked up `addr` as the
        // HashMap key directly, which only succeeded at offset 0, so a
        // fallback full scan over every live arena was added to cover
        // non-zero offsets.
        //
        // PERF FIX (2026-07-25, H2 TestFileSystem.testConcurrent profiling):
        // that O(n)-in-live-arena-count scan was the single dominant cost
        // (~24% of total CPU, `perf record -F 999`, 38854 samples) of a
        // real-disk-I/O workload doing many small Unsafe reads/writes
        // through direct ByteBuffers backed by this arena store — every
        // non-zero-offset byte/short/int/long access re-scanned every live
        // arena. `next_addr` in `allocate()` only ever increases, so live
        // arenas are always disjoint, non-overlapping `[base, base+len)`
        // ranges keyed by their (ordered) base address — exactly what a
        // `BTreeMap` range query answers in O(log n): the arena containing
        // `addr`, if any, is the one with the largest base <= addr.
        fn locate(inner: &BTreeMap<i64, Arena>, addr: i64) -> Option<(i64, usize)> {
            let (&base, arena) = inner.range(..=addr).next_back()?;
            let offset = (addr - base) as u64;
            if offset < arena.bytes.len() as u64 {
                Some((base, offset as usize))
            } else {
                None
            }
        }

        fn read<const N: usize>(&self, addr: i64) -> Option<[u8; N]> {
            let inner = self.inner.read();
            let (base, offset) = Self::locate(&inner, addr)?;
            let arena = inner.get(&base)?;
            let end = offset.checked_add(N)?;
            if end > arena.bytes.len() {
                return None;
            }
            let mut buf = [0u8; N];
            buf.copy_from_slice(&arena.bytes[offset..end]);
            Some(buf)
        }

        /// audit-2026-05-16: returns `true` on success, `false` on
        /// out-of-bounds (do NOT silently grow the arena).
        fn write<const N: usize>(&self, addr: i64, data: &[u8; N]) -> bool {
            let mut inner = self.inner.write();
            let (base, offset) = match Self::locate(&inner, addr) {
                Some(v) => v,
                None => return false,
            };
            let arena = match inner.get_mut(&base) {
                Some(a) => a,
                None => return false,
            };
            let end = match offset.checked_add(N) {
                Some(e) => e,
                None => return false,
            };
            if end > arena.bytes.len() {
                return false;
            }
            arena.bytes[offset..end].copy_from_slice(data);
            true
        }

        /// True if `addr` falls inside any live arena block. Exact membership
        /// (not a range heuristic) — used by native I/O to decide whether a
        /// pointer is an Unsafe-arena handle vs a real OS address.
        ///
        /// R1: every handle carries [`ARENA_TAG`], which no real pointer can
        /// have set, so an untagged `addr` (a real OS pointer) is rejected
        /// up front without taking the lock — this is what makes the
        /// arena/real-pointer classification *provably* unambiguous rather
        /// than relying on the two ranges happening not to overlap. A tagged
        /// `addr` still goes through the exact live-block membership check, so
        /// a freed handle correctly reports `false`.
        pub(super) fn contains(&self, addr: i64) -> bool {
            if addr & ARENA_TAG == 0 {
                return false;
            }
            let inner = self.inner.read();
            Self::locate(&inner, addr).is_some()
        }

        /// The REAL, dereferenceable address of the byte that arena handle
        /// `addr` names, plus how many bytes remain in its block.
        ///
        /// Every other accessor on this store copies, because every other
        /// caller is Rust and can. JNI cannot: a native library that calls
        /// `GetDirectBufferAddress` is handed a `void*` and dereferences it
        /// itself, so a handle — whose whole point is that it is NOT a real
        /// pointer — is a guaranteed SIGSEGV the moment it reaches C. Handing
        /// out the backing pointer is what makes that call answerable at all;
        /// see `jni_get_direct_buffer_address`.
        ///
        /// The pointer is into the block's `Vec<u8>`, so writes through it land
        /// in the arena with no copy-back, which is the aliasing a direct
        /// buffer is supposed to have. It stays valid until that block is
        /// freed or reallocated — `Vec`'s heap buffer does not move when the
        /// `BTreeMap` rebalances around it, only when `reallocate` resizes it.
        /// That is the same lifetime a real `malloc`'d direct buffer gives a
        /// native under HotSpot, and it is the caller's (the JNI spec's)
        /// contract not to outlive it.
        pub(super) fn real_ptr(&self, addr: i64) -> Option<(*mut u8, usize)> {
            if addr & ARENA_TAG == 0 {
                return None;
            }
            let mut inner = self.inner.write();
            let (base, offset) = Self::locate(&inner, addr)?;
            let arena = inner.get_mut(&base)?;
            let remaining = arena.bytes.len().checked_sub(offset)?;
            // SAFETY: `locate` established `offset < arena.bytes.len()`, so the
            // offset is in bounds of the block's own allocation.
            let ptr = unsafe { arena.bytes.as_mut_ptr().add(offset) };
            // A RAW POINTER INTO A `Vec<u8>` IS NOW IN NATIVE HANDS, and both
            // call sites (`jni_long_arg_bits`, `direct_buffer_native_address`)
            // discard `remaining` -- so the callee has a pointer and no bound.
            // Record the block so `reallocate`/`free` can say whether anyone was
            // still holding one when the buffer moved or died. See
            // `unsafe_arena_translation_stats`.
            TRANSLATIONS.fetch_add(1, Ordering::Relaxed);
            *self.translated.lock().entry(base).or_insert(0) += 1;
            Some((ptr, remaining))
        }

        /// Copy `out.len()` bytes OUT of the arena (arena → `out`). Returns
        /// false if the `[addr, addr+len)` range is not fully inside one live
        /// arena block.
        pub(super) fn copy_out(&self, addr: i64, out: &mut [u8]) -> bool {
            // A ZERO-LENGTH copy reads no bytes, so it cannot be out of
            // bounds -- and it is legal at exactly one past the end of the
            // block. `locate` is an EXCLUSIVE range test, so without this
            // it reports "not in any live block" for `base + len` and the
            // caller turns that into a spurious exception. Every direct
            // `ByteBuffer` bulk `get`/`put` of an empty range while
            // positioned at the buffer's limit lands here -- netty's
            // `AbstractByteBufTest.writerIndexBoundaryCheck4` does exactly
            // that (`writeBytes(ByteBuffer.wrap(EMPTY_BYTES))` on a full
            // direct buffer) and threw `IllegalStateException`.
            if out.is_empty() {
                return true;
            }
            let inner = self.inner.read();
            // ONE tree walk: the block with the largest base <= addr is the
            // block and the walk already holds it (`locate` + `get` walked twice).
            let Some((&base, arena)) = inner.range(..=addr).next_back() else {
                return false;
            };
            let offset = (addr - base) as u64;
            if offset >= arena.bytes.len() as u64 {
                return false;
            }
            let offset = offset as usize;
            let end = match offset.checked_add(out.len()) {
                Some(e) => e,
                None => return false,
            };
            if end > arena.bytes.len() {
                return false;
            }
            out.copy_from_slice(&arena.bytes[offset..end]);
            true
        }

        /// Copy `data` INTO the arena (`data` → arena). Symmetric to
        /// [`Self::copy_out`].
        pub(super) fn copy_in(&self, addr: i64, data: &[u8]) -> bool {
            // Zero-length write: see `copy_out` above. Writes nothing, so it
            // is in bounds anywhere, including one past the end of a block.
            if data.is_empty() {
                return true;
            }
            let mut inner = self.inner.write();
            // ONE tree walk, as in `copy_out`.
            let Some((&base, arena)) = inner.range_mut(..=addr).next_back() else {
                return false;
            };
            let offset = (addr - base) as u64;
            if offset >= arena.bytes.len() as u64 {
                return false;
            }
            let offset = offset as usize;
            let end = match offset.checked_add(data.len()) {
                Some(e) => e,
                None => return false,
            };
            if end > arena.bytes.len() {
                return false;
            }
            arena.bytes[offset..end].copy_from_slice(data);
            true
        }
    }

    pub(super) fn store() -> &'static ArenaStore {
        use std::sync::OnceLock;
        static S: OnceLock<ArenaStore> = OnceLock::new();
        S.get_or_init(ArenaStore::new)
    }

    // jr20 wave 9 / gcE: `InvokedCleaners` (a process-wide set of `ByteBuffer`
    // heap addresses, never swept or remapped, whose answer
    // `native_unsafe_invoke_cleaner` ignored) is deleted: a write-only leak
    // (gch-h1ak-raw-address-keyed-audit-RETIRED-UNFIXED-20261018 item 6).
}

/// Allocate an arena block and return its tagged handle.
///
/// `pub` alongside [`unsafe_arena_real_ptr`] and [`unsafe_arena_copy_in`]:
/// production code reaches the arena through the `Unsafe` natives, but the JNI
/// boundary in `cratonvm-vm` needs a live tagged handle to test that
/// `GetDirectBufferAddress` really does bound the pointer it publishes by the
/// capacity `GetDirectBufferCapacity` advertises. That guard is memory safety,
/// and a guard whose wiring is only tested one crate away from where it is
/// wired is a guard nobody has tested.
pub fn unsafe_arena_allocate(size: usize) -> i64 {
    unsafe_arena::store().allocate(size)
}

/// nio21 wave 1 L3: the layout of the JIT-readable arena table (see the
/// section comment above `unsafe_arena::JIT_SLOT_SHIFT`). The optimizing tier
/// bakes the same numbers (`cratonvm_jit::ir_lower::SCOPED_ARENA_*`); the VM's
/// `jit_well_known_class_id` arm asserts at compile time that they agree.
pub const UNSAFE_ARENA_JIT_SLOT_SHIFT: u32 = unsafe_arena::JIT_SLOT_SHIFT;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_INDEX_BITS: u32 = unsafe_arena::JIT_INDEX_BITS;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_ENTRY_SHIFT: u32 = unsafe_arena::JIT_ENTRY_SHIFT;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_KEY_OFFSET: usize = unsafe_arena::JIT_SLOT_KEY_OFFSET;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_LEN_OFFSET: usize = unsafe_arena::JIT_SLOT_LEN_OFFSET;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_PTR_OFFSET: usize = unsafe_arena::JIT_SLOT_PTR_OFFSET;

/// The address of the JIT-readable arena table, or `0` when
/// `CRATONVM_UNSAFE_ARENA_JIT_TABLE=0`. Fixed for the life of the process once
/// non-zero (the store is a never-dropped `OnceLock`, the table is never
/// resized). Asked by the VM's `jit_well_known_class_id` for the optimizing
/// tier's scoped fold.
pub fn unsafe_arena_jit_table_addr() -> usize {
    unsafe_arena::store().jit_table_addr()
}

/// Fallible `allocateMemory` backing: `None` means "the allocation failed",
/// which the native maps to `OutOfMemoryError`. See
/// `unsafe_arena::ArenaStore::try_allocate`.
pub(crate) fn unsafe_arena_try_allocate(size: usize) -> Option<i64> {
    unsafe_arena::store().try_allocate(size)
}

/// Fallible `reallocateMemory` backing. See
/// `unsafe_arena::ArenaStore::try_reallocate`.
pub(crate) fn unsafe_arena_try_reallocate(addr: i64, new_size: usize) -> Option<i64> {
    unsafe_arena::store().try_reallocate(addr, new_size)
}

/// True if `addr` is a live `Unsafe.allocateMemory` arena handle (as opposed
/// to a real OS pointer). NIO native I/O (e.g. `sun/nio/ch/Net.read0/write0`,
/// which live in the `native-io` crate) uses this — via the `NativeContext`
/// bridge — to read/write `DirectByteBuffer` memory that `Util`'s temp-buffer
/// path backs with arena handles instead of raw pointers. Without it,
/// `net_write0` would `memcpy` from a tagged synthetic handle and SIGSEGV.
///
/// R1: handles carry [`unsafe_arena::ARENA_TAG`] (bit 62), which is never set
/// on a real OS pointer, so this is an exact classifier — a real
/// `allocateDirect` pointer can never be mistaken for a handle.
pub fn unsafe_arena_contains(addr: i64) -> bool {
    unsafe_arena::store().contains(addr)
}

/// True if `addr` carries the arena tag bit (bit 62) — i.e. it is an
/// `Unsafe.allocateMemory` handle (LIVE **or** freed), as opposed to a real OS
/// pointer such as a `ByteBuffer.allocateDirect` address from `dbb_allocate`.
///
/// The single-element `Unsafe.get/putX(long)` natives use this to classify a
/// store-rejected address: a *tagged* reject is a freed/out-of-bounds handle
/// and must keep surfacing the use-after-free `IllegalArgumentException`, while
/// an *untagged* reject is a real pointer that should fall through to a raw
/// access (mirroring the `copyMemory` real-pointer path). Unlike
/// [`unsafe_arena_contains`], this is true for freed handles too — it tests the
/// tag, not liveness.
///
/// `pub` because the VM's `copy_from_native_memory` / `copy_to_native_memory`
/// bridge classifies with THIS rather than with `unsafe_arena_contains`: the
/// liveness test sends a freed handle down the raw-pointer branch, where it is
/// dereferenced as an OS address. A tag test refuses it instead, and costs one
/// AND rather than an `RwLock` read plus a `BTreeMap` range probe.
pub fn unsafe_arena_addr_is_tagged(addr: i64) -> bool {
    addr & unsafe_arena::ARENA_TAG != 0
}

/// Translate a live arena handle into a REAL pointer a native library can
/// dereference, together with the bytes remaining in its block. `None` for an
/// untagged address (already a real pointer — nothing to translate) and for a
/// freed or out-of-bounds handle.
///
/// This is the one place the arena's backing store is exposed rather than
/// copied, and it exists for the JNI boundary alone: `GetDirectBufferAddress`
/// must answer with something C can dereference. Every in-VM consumer should
/// keep using [`unsafe_arena_copy_out`] / [`unsafe_arena_copy_in`], which are
/// bounds-checked on every access.
pub fn unsafe_arena_real_ptr(addr: i64) -> Option<(*mut u8, usize)> {
    unsafe_arena::store().real_ptr(addr)
}

/// `r16-ffm-plain-downcall-costs-microseconds` (round 19): the live block
/// holding `addr`, for one downcall that holds the block's session -- see
/// `ArenaStore::peek`. `(real pointer, block real base, block length, block
/// handle base)`.
pub(crate) fn unsafe_arena_peek(addr: i64, want: usize) -> Option<(*mut u8, usize, usize, i64)> {
    unsafe_arena::store().peek(addr, want)
}

/// Translate a live arena handle into a real pointer that is good for at least
/// `want` bytes, or refuse.
///
/// [`unsafe_arena_real_ptr`] returns the bound and every caller used to throw it
/// away. This is the same translation with the bound APPLIED: a JNI caller that
/// is about to publish a pointer alongside an advertised length passes that
/// length here, and a block that cannot cover it answers `None` instead of
/// handing out a pointer the callee will legitimately run off the end of.
///
/// `None` is the right answer at the JNI boundary because the spec already
/// reserves NULL for "not a direct buffer", so natives that check anything
/// check for it — whereas a short pointer is checked by nobody.
pub fn unsafe_arena_real_ptr_bounded(addr: i64, want: usize) -> Option<*mut u8> {
    let (ptr, remaining) = unsafe_arena_real_ptr(addr)?;
    if remaining < want {
        unsafe_arena::SHORT_TRANSLATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            target: "cratonvm::unsafe_arena",
            handle = format!("{addr:#x}"),
            remaining,
            want,
            "refusing to publish a real pointer for an Unsafe-arena block that is SHORTER than the length being advertised with it -- the callee would write past the block"
        );
        return None;
    }
    Some(ptr)
}

/// The lifetime audit for real pointers handed out of the Unsafe arena.
///
/// # Why this exists
///
/// [`unsafe_arena_real_ptr`] is the one place the arena's backing store is
/// exposed rather than copied, and it has two production callers, both in
/// `jni.rs`: `jni_long_arg_bits` (a tagged handle arriving as a JNI `jlong`
/// argument — netty-tcnative's `SSL.bioWrite(long bio, long address, int len)`
/// is the named case) and `direct_buffer_native_address`. Both USED to discard
/// the `remaining` bound, so the callee received a bare pointer into a
/// `Vec<u8>` with no length and nothing checked what it wrote.
///
/// Two ways that becomes a write into unrelated memory, and where each stands:
///
/// * **the callee writes past the block.** `direct_buffer_native_address` now
///   goes through [`unsafe_arena_real_ptr_bounded`] with the buffer's own
///   advertised capacity, so a block that cannot cover it is refused and
///   counted in `short_translations`. `jni_long_arg_bits` cannot be bounded —
///   a `jlong` argument carries no length — and is counted in `translations`
///   only;
/// * **the block is `reallocate`d or `free`d while the pointer is
///   outstanding.** `reallocate` no longer drops the displaced buffer when a
///   pointer is outstanding: it RETAINS it, so the pointer keeps naming memory
///   this process owns (`retained_on_realloc`). `free` still ends the
///   pointer's life, which is what the contract says on any VM.
///
/// # Reading a zero
///
/// `translations` is the DENOMINATOR and is why this is printed rather than
/// just warned about: without it, "the hazard never fired" and "no pointer was
/// ever handed out on this workload" are the same run. `stale_on_*` and
/// `short_translations` are only meaningful beside a non-zero `translations`.
///
/// # Provenance
///
/// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` hunted a writer
/// that puts an 8-byte arena-pointer-shaped value onto a live object's header,
/// and eliminated every managed store path, the allocator, the slide and
/// `copy_to_native_memory`. This path was its last untested row. The crash was
/// later bisected to the collector's own reference processing (`aa4bc7922`),
/// so this is not that writer — but the bound was genuinely being dropped, and
/// that is fixed here rather than left as a probe.
pub fn unsafe_arena_translation_stats() -> ArenaTranslationStats {
    use std::sync::atomic::Ordering;
    ArenaTranslationStats {
        translations: unsafe_arena::TRANSLATIONS.load(Ordering::Relaxed),
        stale_on_realloc: unsafe_arena::STALE_ON_REALLOC.load(Ordering::Relaxed),
        stale_on_free: unsafe_arena::STALE_ON_FREE.load(Ordering::Relaxed),
        retained_on_realloc: unsafe_arena::RETAINED_ON_REALLOC.load(Ordering::Relaxed),
        short_translations: unsafe_arena::SHORT_TRANSLATIONS.load(Ordering::Relaxed),
    }
}

/// The arena real-pointer audit, as one greppable line, at process exit.
///
/// HERE in `native-builtins` and not only in `vm-cli`, for the reason the
/// corrupt-cell census records beside it on the same path: **a JUnit runner
/// leaves through `System.exit` and never reaches `vm-cli`'s normal-return
/// arm.** The netty class this counter was built for --
/// `io.netty.util.ResourceLeakDetectorTest` under
/// `zgc-rewrite-pass-walks-off-a-reference-array-20260815` -- is exactly such a
/// runner, and it exits NON-ZERO on its one failing test. A line printed only
/// on the normal-return path is therefore missing from precisely the runs
/// anyone would look at, and reads as a clean zero.
///
/// Prints when there is something to report, or when GC statistics were asked
/// for. `translations` is the DENOMINATOR: without it, "the hazard never
/// fired" and "no pointer was ever handed out on this workload" are the same
/// run, which is the whole reason the count exists.
///
/// A `Once` keeps the two exit paths from printing it twice. On the
/// `System.exit` path only `CRATONVM_GC_STATS` is consulted -- `--verbose:gc`
/// is a launcher flag and is not reachable from a native.
pub fn arena_translation_exit_summary(always: bool) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let s = unsafe_arena_translation_stats();
    let noteworthy = s.stale_on_realloc != 0 || s.stale_on_free != 0 || s.short_translations != 0;
    // nio21 wave 10 lane L5: a noteworthy count alone prints only under
    // `CRATONVM_DBG=exit-census` (HotSpot's stderr holds no such line); GC
    // stats (`always`) still print the row unconditionally.
    if !always && !(noteworthy && crate::lang_system::exit_census_on()) {
        return;
    }
    ONCE.call_once(|| {
        eprintln!(
            "[VM] arena-ptr: translations={} short_translations={} stale_on_realloc={} retained_on_realloc={} stale_on_free={}",
            s.translations,
            s.short_translations,
            s.stale_on_realloc,
            s.retained_on_realloc,
            s.stale_on_free,
        );
    });
}

/// End one translation taken by [`unsafe_arena_real_ptr`] / [`unsafe_arena_real_ptr_bounded`]
/// for `addr` (the native call that held the pointer has returned).
pub fn unsafe_arena_release_translation(addr: i64) {
    unsafe_arena::store().release_ptr(addr);
}

/// See [`unsafe_arena_translation_stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArenaTranslationStats {
    /// Real pointers handed out of the arena. The denominator: every other
    /// field is unreadable without it.
    pub translations: u64,
    /// Resizes that MOVED a block while a real pointer into it was
    /// outstanding. Each one is also a `retained_on_realloc`.
    pub stale_on_realloc: u64,
    /// Frees of a block while a real pointer into it was outstanding.
    pub stale_on_free: u64,
    /// Displaced buffers kept alive rather than dropped, so a stale pointer
    /// names memory this process still owns.
    pub retained_on_realloc: u64,
    /// Translations REFUSED because the block was shorter than the length
    /// being advertised alongside the pointer.
    pub short_translations: u64,
}

#[cfg(test)]
pub(crate) fn unsafe_arena_block_is_translated(addr: i64) -> bool {
    unsafe_arena::store().block_is_translated(addr)
}

#[cfg(test)]
pub(crate) fn unsafe_arena_block_is_retired(addr: i64) -> bool {
    unsafe_arena::store().block_is_retired(addr)
}

/// Copy bytes out of the Unsafe arena (arena → `out`). Returns false if the
/// range isn't fully inside one live arena block. See [`unsafe_arena_contains`].
pub fn unsafe_arena_copy_out(addr: i64, out: &mut [u8]) -> bool {
    unsafe_arena::store().copy_out(addr, out)
}

/// Copy bytes into the Unsafe arena (`data` → arena). Symmetric to
/// [`unsafe_arena_copy_out`].
pub fn unsafe_arena_copy_in(addr: i64, data: &[u8]) -> bool {
    unsafe_arena::store().copy_in(addr, data)
}

pub(crate) fn unsafe_arena_reallocate(addr: i64, new_size: usize) -> i64 {
    unsafe_arena::store().reallocate(addr, new_size)
}

/// Free an arena block. `pub` for the same reason [`unsafe_arena_allocate`] is.
pub fn unsafe_arena_free(addr: i64) {
    unsafe_arena::store().free(addr);
}

pub(crate) fn unsafe_arena_get_byte(addr: i64) -> u8 {
    unsafe_arena::store().get_byte(addr)
}

// audit-round5 fix #3 (HIGH): bounds-checked get* shims. Return `Some(v)`
// when the address falls inside a live arena, `None` otherwise. The
// get-at-address natives map `None` to IllegalArgumentException to match
// the put-side behavior fixed in round-4 wave-1.
pub(crate) fn unsafe_arena_try_get_byte(addr: i64) -> Option<u8> {
    unsafe_arena::store().try_get_byte(addr)
}

/// audit-2026-05-16: returns `true` on success, `false` when the write
/// would extend the arena past its original size. Java natives map `false`
/// to `IllegalArgumentException`.
pub(crate) fn unsafe_arena_put_byte(addr: i64, v: u8) -> bool {
    unsafe_arena::store().put_byte(addr, v)
}

pub(crate) fn unsafe_arena_get_short(addr: i64) -> i16 {
    unsafe_arena::store().get_short(addr)
}

pub(crate) fn unsafe_arena_try_get_short(addr: i64) -> Option<i16> {
    unsafe_arena::store().try_get_short(addr)
}

pub(crate) fn unsafe_arena_put_short(addr: i64, v: i16) -> bool {
    unsafe_arena::store().put_short(addr, v)
}

pub(crate) fn unsafe_arena_get_int(addr: i64) -> i32 {
    unsafe_arena::store().get_int(addr)
}

pub(crate) fn unsafe_arena_try_get_int(addr: i64) -> Option<i32> {
    unsafe_arena::store().try_get_int(addr)
}

pub(crate) fn unsafe_arena_put_int(addr: i64, v: i32) -> bool {
    unsafe_arena::store().put_int(addr, v)
}

pub(crate) fn unsafe_arena_get_long(addr: i64) -> i64 {
    unsafe_arena::store().get_long(addr)
}

pub(crate) fn unsafe_arena_try_get_long(addr: i64) -> Option<i64> {
    unsafe_arena::store().try_get_long(addr)
}

pub(crate) fn unsafe_arena_put_long(addr: i64, v: i64) -> bool {
    unsafe_arena::store().put_long(addr, v)
}

/// Unsafe.throwException(Throwable) — throw a checked exception unchecked.
fn native_unsafe_throw_exception(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match args.get(1) {
        Some(Value::Object(Some(exc))) => Err(MethodCallFailed::ExceptionThrown(*exc)),
        // A null throwable cannot be thrown. HotSpot 25.0.4+7 does not refuse
        // it either -- it SIGSEGVs inside `Unsafe_ThrowException` -- so this is
        // not "match the oracle"; the oracle has no answer here. Returning
        // quietly is the one option that is certainly wrong: the caller wrote
        // `throwException(x)` expecting control not to reach the next line.
        Some(Value::Object(None)) => Err(RuntimeError::NullPointerException {
            message: Some("Unsafe.throwException: null throwable".to_string()),
        }
        .into()),
        _ => Ok(None),
    }
}

// ===========================================================================
// Session 10: New native method implementations for native bridging
// ===========================================================================

/// Unsafe.objectFieldOffset1(Class, String) — lookup field offset by name.
/// JDK 25 uses this when the Field object is not available.
pub(crate) fn native_unsafe_object_field_offset1(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0]=Unsafe this, args[1]=Class mirror, args[2]=field name String
    let field_name = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Long(0))),
    };
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Long(0))),
    };
    // Resolve the Class mirror's ClassId. Real-JDK Class layout doesn't
    // have classId in slot 0 (slot 0 is `cachedConstructor` or similar
    // ObjectRef), so we go through the reverse-map registered when the
    // mirror is allocated. The synthetic-jdk path keeps slot 0 = Int(cid)
    // and is handled by the fallback below.
    let resolved_cid: Option<cratonvm_types::ClassId> = ctx
        .class_id_from_mirror(class_mirror)
        .or_else(|| match ctx.get_field(class_mirror, 0) {
            // `cid >= 0` matters: a PRIMITIVE mirror carries `Int(-1)` in slot 0
            // as its marker, and without this guard that decodes to
            // `ClassId(0xFFFF_FFFF)` — a plausible-looking id for a class that
            // does not exist. `mirror_class_id` in lang_class.rs has always had
            // the guard; this copy did not.
            Value::Int(cid) if cid >= 0 => Some(cratonvm_types::ClassId::new(cid as u32)),
            _ => None,
        });
    if let Some(class_id) = resolved_cid {
        if let Some(cname) = ctx.class_name_of_id(class_id) {
            if let Some(slot) = ctx.resolve_field_index(&cname, &field_name) {
                // Wave 8c: a byte offset under byte field offsets.
                let encoded = unsafe_encode_field_offset(ctx.unsafe_byte_field_offsets(), slot);
                return Ok(Some(Value::Long(encoded as i64)));
            }
        }

        // T19.H1 — `declared_fields` only lists fields declared by
        // `class_id` itself, not inherited ones. HotSpot resolves
        // `Unsafe.objectFieldOffset(C.class, "name")` by walking the
        // inheritance chain; we must do the same, otherwise a field
        // inherited from a superclass (e.g. AbstractMap.keySet) is
        // invisible and we return 0, which collides with the object
        // header / first slot and causes `Unsafe.compareAndSetInt` to
        // read the wrong storage — livelocking every CAS loop built on
        // top of it (the original KC16 + KC26 hang at
        // ConcurrentHashMap.initTable).
        let mut cid_opt = Some(class_id);
        while let Some(cid) = cid_opt {
            let fields = ctx.declared_fields(cid);
            for f in &fields {
                if !f.is_static && f.name == field_name {
                    // Wave 8c: a byte offset under byte field offsets.
                    let encoded =
                        unsafe_encode_field_offset(ctx.unsafe_byte_field_offsets(), f.slot_index);
                    return Ok(Some(Value::Long(encoded as i64)));
                }
            }
            cid_opt = ctx.superclass_of(cid);
        }
        // Static fields live in a separate slot space. Only walk them
        // if the caller explicitly asked for a static via
        // `staticFieldOffset` — instance lookups stop at the chain end.
        if field_name.starts_with("static:") {
            // Conservative — HotSpot's staticFieldOffset is a distinct
            // entry point; this branch is defensive so we don't silently
            // fall through to zero and reintroduce the livelock if a
            // caller mis-dispatches.
            let real_name = &field_name["static:".len()..];
            let mut cid_opt = Some(class_id);
            while let Some(cid) = cid_opt {
                let fields = ctx.declared_fields(cid);
                for f in &fields {
                    if f.is_static && f.name == real_name {
                        return Ok(Some(Value::Long(f.slot_index as i64)));
                    }
                }
                cid_opt = ctx.superclass_of(cid);
            }
        }
    }
    // Lookup failed.  Returning `0` here used to alias slot 0 of the
    // receiver and livelock the caller's CAS loop (see WildFly's
    // `Class$Atomic.casReflectionData` and the Spring Boot
    // `AbstractClassLoaderValue.putIfAbsent` watchdog hangs).  Mint a
    // unique non-zero synthetic offset instead — the Unsafe.{CAS,get,put}
    // natives detect the synthetic range and route through a per-object
    // side store (`synthetic_field_store`) that yields self-consistent
    // load/CAS/store semantics.  This unblocks lazy-init guards whose
    // backing field happens to live on a class our layout doesn't
    // expose (typically `java.lang.Class`'s synthetic
    // `reflectionData` / `annotationData` / `annotationType` slots, or
    // ConcurrentHashMap's `table` reference when the populator failed
    // to wire the rj_slot metadata).
    let cname = resolved_cid
        .and_then(|cid| ctx.class_name_of_id(cid))
        .unwrap_or_default();
    // A STATIC field, asked for by its plain name. HotSpot accepts this at
    // this door and CratonVM must too -- measured: `internal oFO(Class,String)
    // static name` is `no-throw` on HotSpot.
    //
    // The `static:`-prefixed branch above is for a caller that mis-dispatches;
    // this is the ordinary spelling. Registering the offset (rather than
    // merely numbering it, which is what the old mint did) is what makes a
    // later get/CAS reach the REAL static instead of a phantom slot -- the
    // same distinction the memory-access latch repair turned on.
    if let Some(cid0) = resolved_cid {
        let mut cid_opt = Some(cid0);
        while let Some(cid) = cid_opt {
            let mut static_idx = 0usize;
            for f in &ctx.declared_fields(cid) {
                if f.is_static {
                    if f.name == field_name {
                        let cname = ctx.class_name_of_id(cid).unwrap_or_default();
                        // Wave 103, L2: keyed by the class too.
                        let offset =
                            crate::static_field_synthetic_offset(&cname, &field_name, cid);
                        let vm = ctx.vm_identity();
                        remember_unsafe_static_field_offset(vm, offset, cid, static_idx);
                        return Ok(Some(Value::Long(offset as i64)));
                    }
                    static_idx += 1;
                }
            }
            cid_opt = ctx.superclass_of(cid);
        }
    }

    // REFUSE, as HotSpot does. This used to mint a synthetic offset and route
    // it through a per-object side store, so a lazy-init guard ran against a
    // phantom field and its CAS reported success against nothing any reader
    // can see.
    //
    // The mint's two documented consumers were measured away (record §14:
    // `Class$Atomic` resolves its three real names on JDK 25;
    // `AbstractClassLoaderValue` references `Unsafe` zero times), and the path
    // takes 0 mints across 120 regression vectors, 136 Netty buffer/util
    // classes, both DoD workloads and both H2 vectors -- each with a firing
    // positive control (`MintReachProbe`, 2).
    //
    // WildFly/Keycloak/Elasticsearch are not runnable on this host. If one of
    // them does reach here, this now names the class and the field in a
    // catchable `InternalError` instead of diverging silently.
    Err(RuntimeError::InternalError {
        message: format!("no such field {field_name:?} on class {cname:?}"),
    }
    .into())
}

pub(crate) fn unsafe_compare_exchange_static_field(
    ctx: &mut dyn NativeContext,
    offset: usize,
    expected: Value,
    update: Value,
) -> Option<Value> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    let mut attempts = 0usize;
    loop {
        let old = ctx.get_static_field(class_id, field_index);
        if !unsafe_cas_values_equal(old.clone(), expected.clone()) {
            return Some(old);
        }
        if unsafe_static_cas(ctx, offset, expected.clone(), update.clone()).unwrap_or(false) {
            return Some(old);
        }
        attempts = attempts.wrapping_add(1);
        if attempts % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
}

pub(crate) fn unsafe_compare_exchange_synthetic_field(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    expected: Value,
    update: Value,
) -> Value {
    let mut attempts = 0usize;
    loop {
        let old = synthetic_get(ctx, obj, offset);
        if !unsafe_cas_values_equal(old.clone(), expected.clone()) {
            return old;
        }
        if synthetic_cas(ctx, obj, offset, expected.clone(), update.clone()) {
            return old;
        }
        attempts = attempts.wrapping_add(1);
        if attempts % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
}

pub(crate) fn unsafe_compare_exchange_heap_slot(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    index: usize,
    expected: Value,
    update: Value,
) -> Value {
    let is_array = ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array;
    let mut attempts = 0usize;
    loop {
        let old = if is_array {
            ctx.get_array_element(obj, index)
        } else {
            let value = ctx.get_field_volatile(obj, index);
            // A failed exchange still hands a strong witness to Java.
            slot0_read_keep_alive(ctx, obj, index, value)
        };
        if !unsafe_cas_values_equal(old.clone(), expected.clone()) {
            return old;
        }
        // Interpreter round i6 wave 127, lane L4: a `float` / `double` slot
        // that matched an int / long expectation by its bits is exchanged
        // as itself: the CAS expects the `Value::Float` / `Value::Double` it
        // just read (same bits) and stores the update re-tagged, so a
        // context whose CAS compares tags strictly still lands it, and the
        // slot keeps its own kind.
        let (cas_expected, cas_update) = match (&old, &expected) {
            (Value::Float(_), Value::Int(_)) | (Value::Double(_), Value::Long(_)) => {
                (old, unsafe_retag_bits_like(&old, update))
            }
            _ => (expected, update),
        };
        if ctx.compare_and_swap_field(obj, index, cas_expected, cas_update) {
            return old;
        }
        attempts = attempts.wrapping_add(1);
        if attempts % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
}

/// Unsafe.compareAndExchangeInt atomically sets field to update if current == expected,
/// returns the witness value (old value).
pub(crate) fn native_unsafe_compare_and_exchange_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Raw bytes for an int-width exchange over another element type; see
    // [`native_unsafe_cas_int`].
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Int, 4)
    {
        let (ex, nv) = (
            unsafe_wide_arg_bits(args, 3, 4),
            unsafe_wide_arg_bits(args, 4, 4),
        );
        if let Some((cur, _)) = unsafe_wide_raw_cas(ctx, obj, off, 4, ex, nv) {
            return Ok(Some(Value::Int(cur as u32 as i32)));
        }
    }
    native_unsafe_compare_and_exchange_int_element(ctx, args)
}

/// [`native_unsafe_compare_and_exchange_int`] without the raw-bytes arm (the
/// byte/short exchanges' fallback).
pub(crate) fn native_unsafe_compare_and_exchange_int_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cae, 4) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Cae, 4) {
        return Ok(Some(v));
    }
    let obj = unsafe_obj(args, 1);
    let offset = unsafe_offset(args, 2);
    let expected = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let update = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let expected_value = Value::Int(expected);
    let update_value = Value::Int(update);
    if obj.is_none() {
        if let Some(old) = unsafe_compare_exchange_static_field(
            ctx,
            offset,
            expected_value.clone(),
            update_value.clone(),
        ) {
            return Ok(Some(match old {
                Value::Int(v) => Value::Int(v),
                // Wave 127 (lane L4): a `float` slot answers its bits.
                Value::Float(f) => Value::Int(f.to_bits() as i32),
                _ => Value::Int(expected),
            }));
        }
    }
    if let Some(obj_ref) = obj {
        let old = if unsafe_synthetic_field_offset(&*ctx, obj_ref, offset) {
            unsafe_compare_exchange_synthetic_field(
                ctx,
                obj_ref,
                offset,
                expected_value,
                update_value,
            )
        } else {
            let index = if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                match unsafe_checked_array_index(ctx, obj_ref, offset) {
                    Some(i) => i,
                    None => return Ok(Some(Value::Int(expected))),
                }
            } else {
                // Wave 8c: the slot (an offset inside a field took the lane above).
                unsafe_field_slot(&*ctx, obj_ref, offset).0
            };
            unsafe_compare_exchange_heap_slot(ctx, obj_ref, index, expected_value, update_value)
        };
        return Ok(Some(match old {
            Value::Int(v) => Value::Int(v),
            // Wave 127 (lane L4): a `float` slot answers its bits.
            Value::Float(f) => Value::Int(f.to_bits() as i32),
            _ => Value::Int(expected),
        }));
    }
    Ok(Some(Value::Int(expected)))
}

/// Unsafe.compareAndExchangeLong atomically sets field to update if current == expected.
pub(crate) fn native_unsafe_compare_and_exchange_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cae, 8) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(args, ArenaAtomic::Cae, 8) {
        return Ok(Some(v));
    }
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Long, 8)
    {
        let (ex, nv) = (
            unsafe_wide_arg_bits(args, 3, 8),
            unsafe_wide_arg_bits(args, 4, 8),
        );
        if let Some((cur, _)) = unsafe_wide_raw_cas(ctx, obj, off, 8, ex, nv) {
            return Ok(Some(Value::Long(cur as i64)));
        }
    }
    let obj = unsafe_obj(args, 1);
    let offset = unsafe_offset(args, 2);
    let expected = match args.get(3) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let update = match args.get(4) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let expected_value = Value::Long(expected);
    let update_value = Value::Long(update);
    if obj.is_none() {
        if let Some(old) = unsafe_compare_exchange_static_field(
            ctx,
            offset,
            expected_value.clone(),
            update_value.clone(),
        ) {
            return Ok(Some(match old {
                Value::Long(v) => Value::Long(v),
                // Wave 127 (lane L4): a `double` slot answers its bits.
                Value::Double(d) => Value::Long(d.to_bits() as i64),
                _ => Value::Long(expected),
            }));
        }
    }
    if let Some(obj_ref) = obj {
        let old = if unsafe_synthetic_field_offset(&*ctx, obj_ref, offset) {
            unsafe_compare_exchange_synthetic_field(
                ctx,
                obj_ref,
                offset,
                expected_value,
                update_value,
            )
        } else {
            let index = if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                match unsafe_checked_array_index(ctx, obj_ref, offset) {
                    Some(i) => i,
                    None => return Ok(Some(Value::Long(expected))),
                }
            } else {
                // Wave 8c: the slot (an offset inside a field took the lane above).
                unsafe_field_slot(&*ctx, obj_ref, offset).0
            };
            unsafe_compare_exchange_heap_slot(ctx, obj_ref, index, expected_value, update_value)
        };
        return Ok(Some(match old {
            Value::Long(v) => Value::Long(v),
            // Wave 127 (lane L4): a `double` slot answers its bits.
            Value::Double(d) => Value::Long(d.to_bits() as i64),
            _ => Value::Long(expected),
        }));
    }
    Ok(Some(Value::Long(expected)))
}

/// Unsafe.compareAndExchangeReference atomically sets field to update if current == expected.
pub(crate) fn native_unsafe_compare_and_exchange_reference(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = unsafe_obj(args, 1);
    let offset = unsafe_offset(args, 2);
    let expected = recover_object_arg(args.get(3).copied().unwrap_or(Value::Object(None)));
    let update = recover_object_arg(args.get(4).copied().unwrap_or(Value::Object(None)));
    if obj.is_none() {
        if let Some(old) =
            unsafe_compare_exchange_static_field(ctx, offset, expected.clone(), update.clone())
        {
            return Ok(Some(recover_object_arg(old)));
        }
    }
    if let Some(obj_ref) = obj {
        let old = if unsafe_synthetic_field_offset(&*ctx, obj_ref, offset) {
            unsafe_compare_exchange_synthetic_field(ctx, obj_ref, offset, expected, update)
        } else {
            let index = if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                match unsafe_checked_array_index(ctx, obj_ref, offset) {
                    Some(i) => i,
                    None => return Ok(Some(Value::Object(None))),
                }
            } else {
                // Wave 8c: the slot; an offset inside a field names no
                // reference (no exchange).
                match unsafe_field_slot(&*ctx, obj_ref, offset) {
                    (_, k) if k != 0 => return Ok(Some(Value::Object(None))),
                    (slot, _) => slot,
                }
            };
            unsafe_compare_exchange_heap_slot(ctx, obj_ref, index, expected, update)
        };
        return Ok(Some(recover_object_arg(old)));
    }
    Ok(Some(Value::Object(None)))
}

/// The arena handle → real pointer translation that `GetDirectBufferAddress`
/// depends on. Written against the *observable* the JNI boundary needs: a
/// pointer that can be dereferenced from C, that aliases the arena rather than
/// copying it, and that refuses a handle no live block owns.
/// Serializes the tests whose assertions depend on the process-wide arena's
/// layout (what lies at a block's end, whether a grow can reach the next
/// block). Run in parallel, one test's block could start exactly at another's
/// `end` and make its "a write past the end is refused" check fail: seen 2
/// runs in 10 at the gc-common w8 verification.
#[cfg(test)]
fn arena_layout_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod unsafe_arena_real_ptr_tests {
    use super::*;

    /// **A block resized or freed while native code holds its real pointer is
    /// reported.**
    ///
    /// This is the hazard `unsafe_arena_translation_stats` exists for, and it can
    /// only be asserted on bookkeeping: the dangling pointer is in a native
    /// library's hands, the write happens outside this process's Rust code, and
    /// by the time anything notices, the evidence is an unrelated object's
    /// corrupted header a collection later. That is the trail
    /// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` follows
    /// backwards.
    ///
    /// # Asserted PER BLOCK, not on the counters
    ///
    /// The counters and the arena store are process-global and cargo runs tests
    /// in parallel threads, so a sibling test translating or freeing its own
    /// block moves them between any baseline and any assertion. The first
    /// version of this asserted exact counter deltas, passed alone, and failed in
    /// the full suite -- the ordinary shape of a global-state test, and worth the
    /// note. Per-block queries are race-free.
    #[test]
    fn resizing_or_freeing_a_translated_block_is_reported() {
        let _arena = super::arena_layout_test_lock();
        // A block nobody has translated must NOT be recorded -- a detector that
        // fires on every free names nothing.
        let quiet = unsafe_arena_allocate(64);
        assert!(
            !unsafe_arena_block_is_translated(quiet),
            "an untranslated block must not be recorded"
        );
        let quiet = unsafe_arena_try_reallocate(quiet, 128).expect("resize succeeds");
        assert!(
            !unsafe_arena_block_is_translated(quiet),
            "and resizing it must not make it recorded"
        );
        unsafe_arena_free(quiet);

        // Handing the pointer out records it, and the bound EXISTS here -- both
        // production callers drop it, which is the defect this instruments.
        let held = unsafe_arena_allocate(64);
        let (_p, remaining) = unsafe_arena_real_ptr(held).expect("live handle translates");
        assert_eq!(remaining, 64, "the bound exists at the source");
        assert!(
            unsafe_arena_block_is_translated(held),
            "handing a real pointer to native code must be recorded -- otherwise `reallocate` cannot tell that it is about to move a buffer somebody is holding"
        );

        // Resizing it is the hazard: `Vec::resize` may move the buffer.
        let r_before = unsafe_arena_translation_stats().stale_on_realloc;
        let held = unsafe_arena_try_reallocate(held, 4096).expect("resize succeeds");
        let r_after = unsafe_arena_translation_stats().stale_on_realloc;
        assert!(
            r_after > r_before,
            "resizing a block whose real pointer is outstanding must be reported"
        );

        // Freeing clears the record, so a recycled handle does not inherit it.
        let doomed = unsafe_arena_allocate(64);
        let _ = unsafe_arena_real_ptr(doomed).expect("translates");
        unsafe_arena_free(doomed);
        assert!(
            !unsafe_arena_block_is_translated(doomed),
            "the record must be dropped with the block, or a later handle at the same address inherits a warning that is not about it"
        );

        unsafe_arena_free(held);
    }

    #[test]
    fn a_live_handle_translates_to_a_pointer_that_aliases_the_arena() {
        let _arena = super::arena_layout_test_lock();
        let handle = unsafe_arena_allocate(64);
        assert!(
            unsafe_arena_addr_is_tagged(handle),
            "handle {handle:#x} must carry the arena tag — the whole classification rests on it"
        );

        let (ptr, remaining) = unsafe_arena_real_ptr(handle).expect("live handle translates");
        assert_eq!(remaining, 64);
        assert!(
            !unsafe_arena_addr_is_tagged(ptr as i64),
            "the translated pointer must be a real address, not the handle again"
        );

        // Native-side write, arena-side read: this is the direction that was
        // SIGSEGV-ing, and the aliasing is the point — a copy would pass the
        // write test and still leave the buffer's contents wrong.
        // SAFETY: `remaining` says 64 bytes are owned by this block.
        unsafe {
            std::ptr::write(ptr.add(3), 0xAB);
        }
        assert_eq!(unsafe_arena_get_byte(handle + 3), 0xAB);

        // Arena-side write, native-side read: the other direction.
        assert!(unsafe_arena_put_byte(handle + 5, 0xCD));
        // SAFETY: same block, offset 5 < 64.
        assert_eq!(unsafe { std::ptr::read(ptr.add(5)) }, 0xCD);

        unsafe_arena_free(handle);
    }

    #[test]
    fn a_mid_block_handle_translates_to_the_matching_interior_pointer() {
        let _arena = super::arena_layout_test_lock();
        let handle = unsafe_arena_allocate(32);
        let (base, base_len) = unsafe_arena_real_ptr(handle).expect("base translates");
        let (mid, mid_len) = unsafe_arena_real_ptr(handle + 8).expect("interior translates");
        assert_eq!(base_len, 32);
        assert_eq!(mid_len, 24, "remaining must count from the interior offset");
        assert_eq!(mid as usize - base as usize, 8);
        unsafe_arena_free(handle);
    }

    #[test]
    fn a_real_pointer_and_a_dead_handle_both_refuse() {
        let _arena = super::arena_layout_test_lock();
        // Untagged: already a real pointer (a native's own NewDirectByteBuffer
        // allocation). Nothing to translate, and translating would be wrong.
        let mut real = [0u8; 8];
        assert!(unsafe_arena_real_ptr(real.as_mut_ptr() as i64).is_none());

        // Freed: a use-after-free must not be handed a pointer into whatever
        // the allocator reused, so it resolves to None and JNI answers NULL.
        let handle = unsafe_arena_allocate(16);
        assert!(unsafe_arena_real_ptr(handle).is_some());
        unsafe_arena_free(handle);
        assert!(unsafe_arena_real_ptr(handle).is_none());

        // Past the end of a live block: same refusal, so a native cannot walk
        // off one buffer into the next. Size 8 on purpose — `allocate` rounds
        // the address bump to 16, so `live + 8 .. live + 15` is guaranteed to
        // belong to no block at all, even though this arena is process-global
        // and other tests allocate into it concurrently.
        let live = unsafe_arena_allocate(8);
        assert!(unsafe_arena_real_ptr(live + 8).is_none());
        unsafe_arena_free(live);
    }

    /// **Growing a block past its reserved handle space must not swallow the
    /// handles issued after it.**
    ///
    /// `try_allocate` bumps `next_addr` by the rounded request, so a block owns
    /// exactly that much handle space, and `locate` resolves an address to the
    /// greatest base at or below it. Growing `bytes` past that made the block
    /// answer for its successors' handles: a 64-byte block grown to 4096
    /// swallowed the next block whole, and every read and write through the
    /// later handle landed silently in the earlier block. Two live
    /// `Unsafe.allocateMemory` allocations, one address range.
    ///
    /// The write-through-both-and-read-back-both shape is deliberate: a test
    /// that only checks the later block reads its own value passes while the
    /// earlier block is being corrupted underneath it.
    #[test]
    fn growing_a_block_must_not_swallow_the_handles_issued_after_it() {
        let _arena = super::arena_layout_test_lock();
        let a = unsafe_arena_allocate(64);
        let b = unsafe_arena_allocate(64);
        assert!(b > a, "handles bump upward");
        // Sized from the ACTUAL gap rather than a constant: this arena is
        // process-global and cargo runs tests in parallel, so a sibling's
        // allocation lands between these two and a fixed 4096 stops spanning
        // `b`. A precondition that a race can make vacuous is worse than none.
        //
        // nio21 wave 1 L3: two SLOTTED handles are a whole 2^30-byte window
        // apart, so the gap no longer measures a grow that reaches `b` (and
        // would allocate a gigabyte); any grow past `a`'s 64 reserved bytes is
        // the case then, and 4096 is one.
        let gap = (b - a) as usize;
        let span = if gap < (1 << 20) { gap + 128 } else { 4096 };

        let grown = unsafe_arena_try_reallocate(a, span).expect("resize succeeds");
        assert_ne!(
            grown, a,
            "a grow that its reserved handle space cannot hold must relocate"
        );

        assert!(unsafe_arena_put_byte(b + 1, 0x77));
        assert!(unsafe_arena_put_byte(grown + 1, 0x11));
        assert_eq!(
            unsafe_arena_get_byte(b + 1),
            0x77,
            "b must keep its own bytes"
        );
        assert_eq!(
            unsafe_arena_get_byte(grown + 1),
            0x11,
            "and the grown block must keep its own"
        );

        // The old handle is dead, which is `realloc(3)`'s contract and what
        // keeps a use-after-realloc resolving to nothing rather than to
        // whichever block now covers that address.
        assert!(unsafe_arena_real_ptr(a).is_none());
        assert!(!unsafe_arena_put_byte(a, 0xFF));

        unsafe_arena_free(grown);
        unsafe_arena_free(b);
    }

    /// **A grow that DOES fit the reserved handle space keeps its handle.** The
    /// negative half: relocating every grow would be correct and would also
    /// make the test above pass for the wrong reason, while needlessly
    /// invalidating handles Java is entitled to keep using.
    #[test]
    fn a_grow_inside_the_reserved_handle_space_keeps_its_handle() {
        let _arena = super::arena_layout_test_lock();
        // 60 bytes reserves 64 -- `next_addr` bumps by the 16-rounded request.
        let handle = unsafe_arena_allocate(60);
        assert!(unsafe_arena_put_byte(handle + 3, 0x2B));
        let same = unsafe_arena_try_reallocate(handle, 64).expect("resize succeeds");
        assert_eq!(same, handle, "a grow that fits must not move the handle");
        assert_eq!(unsafe_arena_get_byte(handle + 3), 0x2B);
        unsafe_arena_free(handle);
    }

    /// **A move under an outstanding real pointer keeps the displaced buffer
    /// alive, and the outstanding pointer still reads it.**
    ///
    /// The assertion that matters is the read through the stale pointer AFTER
    /// the move. Before the retention it named memory the process allocator had
    /// taken back, so the read was undefined and a write through it was an
    /// unbounded corruption of whatever landed there next; now it names a
    /// buffer this process still owns, still holding the pre-move bytes.
    ///
    /// Run this one under Miri or ASan and the difference is the whole point:
    /// the old code is a use-after-free here, this one is not.
    ///
    /// # Asserted PER BLOCK wherever possible
    ///
    /// The counters are process-global and cargo runs tests in parallel, so a
    /// sibling moving its own block lands between any baseline and any
    /// assertion. Counter checks are therefore `>=`, and the load-bearing
    /// assertions are per-block queries, which are race-free. The module's
    /// first attempt at exact deltas passed alone and failed in the suite.
    #[test]
    fn a_move_under_an_outstanding_pointer_retains_the_displaced_buffer() {
        let _arena = super::arena_layout_test_lock();
        let handle = unsafe_arena_allocate(64);
        assert!(unsafe_arena_put_byte(handle + 7, 0x5A));
        let (ptr, remaining) = unsafe_arena_real_ptr(handle).expect("live handle translates");
        assert_eq!(remaining, 64);

        let retained_before = unsafe_arena_translation_stats().retained_on_realloc;
        let moved = unsafe_arena_try_reallocate(handle, 1 << 20).expect("resize succeeds");
        assert_ne!(
            moved, handle,
            "1 MiB cannot fit the 64 bytes of handle space"
        );
        assert!(
            unsafe_arena_translation_stats().retained_on_realloc > retained_before,
            "a move under an outstanding pointer must retain the displaced buffer"
        );
        assert!(
            unsafe_arena_block_is_retired(moved),
            "the retained buffer must be keyed by the handle the block ended up at, \
             or `free` cannot drop it"
        );
        assert!(
            !unsafe_arena_block_is_retired(handle),
            "and not by the dead one, which nothing will ever free"
        );

        // The stale pointer still names owned memory holding the old bytes.
        // SAFETY: this is exactly the dereference the retention makes sound --
        // the displaced buffer is alive and at least 64 bytes long.
        assert_eq!(unsafe { std::ptr::read(ptr.add(7)) }, 0x5A);

        // The block itself moved on, and carried its contents.
        assert_eq!(unsafe_arena_get_byte(moved + 7), 0x5A);

        unsafe_arena_free(moved);
        assert!(
            !unsafe_arena_block_is_retired(moved),
            "freeing the handle must drop the retained buffer -- otherwise an \
             allocate/translate/resize/free loop leaks one buffer per iteration"
        );
    }

    /// **A resize that cannot move the buffer retains nothing.** The negative
    /// half: without it the retention fires on every resize of every translated
    /// block, and the count says nothing about the hazard.
    #[test]
    fn a_resize_that_cannot_move_the_buffer_retains_nothing() {
        let _arena = super::arena_layout_test_lock();
        let handle = unsafe_arena_allocate(4096);
        let _ = unsafe_arena_real_ptr(handle).expect("translates");
        // Shrinking never reallocates and fits the reserved space, so the block
        // neither moves in memory nor changes handle.
        let same = unsafe_arena_try_reallocate(handle, 16).expect("resize succeeds");
        assert_eq!(same, handle);
        assert!(
            !unsafe_arena_block_is_retired(handle),
            "a shrink displaces nothing, so there is nothing to retain"
        );
        unsafe_arena_free(handle);
    }

    /// **The resize path must not move a translated block before it can notice.**
    ///
    /// `try_reallocate`'s capacity probe used to run `try_reserve_exact` on the
    /// LIVE `Vec`, which reallocates -- so the buffer moved during the probe,
    /// and the detection downstream then read a capacity that had just been
    /// satisfied and concluded nothing had moved. An instrument armed where it
    /// could not fire. The probe is gone; this is what proves it stays gone.
    #[test]
    fn the_resize_path_does_not_move_a_translated_block_before_noticing() {
        let _arena = super::arena_layout_test_lock();
        let handle = unsafe_arena_allocate(64);
        let (ptr, _) = unsafe_arena_real_ptr(handle).expect("translates");
        // SAFETY: 64 bytes are owned by this block.
        unsafe { std::ptr::write(ptr.add(1), 0xC3) };

        let before = unsafe_arena_translation_stats().retained_on_realloc;
        let moved = unsafe_arena_try_reallocate(handle, 1 << 20).expect("resize succeeds");
        assert!(
            unsafe_arena_translation_stats().retained_on_realloc > before,
            "the growth must be detected as a move, not hidden by a probe"
        );
        assert!(unsafe_arena_block_is_retired(moved));
        // SAFETY: sound because the displaced buffer was retained.
        assert_eq!(unsafe { std::ptr::read(ptr.add(1)) }, 0xC3);
        unsafe_arena_free(moved);
    }

    /// **A bounded translation refuses a block that cannot cover the advertised
    /// length, and accepts one that can.**
    ///
    /// Both halves are asserted: a guard that refuses everything is as useless
    /// as one that refuses nothing, and `remaining == want` is the boundary the
    /// JNI contract actually names -- a native may touch `capacity` bytes, not
    /// `capacity + 1`.
    #[test]
    fn a_bounded_translation_refuses_only_a_block_that_is_too_short() {
        let _arena = super::arena_layout_test_lock();
        let handle = unsafe_arena_allocate(64);
        assert!(
            unsafe_arena_real_ptr_bounded(handle, 64).is_some(),
            "a block that exactly covers the advertised length must be published"
        );
        assert!(
            unsafe_arena_real_ptr_bounded(handle, 0).is_some(),
            "a zero-length buffer is legal and must not be refused"
        );

        let short_before = unsafe_arena_translation_stats().short_translations;
        assert!(
            unsafe_arena_real_ptr_bounded(handle, 65).is_none(),
            "one byte past the block must be refused -- that byte is the bug"
        );
        assert!(
            unsafe_arena_translation_stats().short_translations > short_before,
            "and the refusal must be counted, or the guard is invisible"
        );

        // From an interior handle the bound counts from the offset, so the same
        // block refuses a length it accepted from its base.
        assert!(unsafe_arena_real_ptr_bounded(handle + 32, 32).is_some());
        assert!(unsafe_arena_real_ptr_bounded(handle + 32, 33).is_none());

        unsafe_arena_free(handle);
        assert!(
            unsafe_arena_real_ptr_bounded(handle, 1).is_none(),
            "a dead handle refuses regardless of length"
        );
    }
}

/// nio21 wave 1 L3: the JIT-readable arena table (the section comment above
/// `unsafe_arena::JIT_SLOT_SHIFT`). `jit_resolve` is the optimizing tier's
/// inline resolve step for step, so these pin what compiled code answers.
#[cfg(test)]
mod nio21_arena_jit_table_tests {
    use super::*;

    fn on() -> bool {
        unsafe_arena_jit_table_addr() != 0
    }

    fn real(addr: i64, want: usize) -> usize {
        unsafe_arena::store()
            .peek(addr, want)
            .map(|(p, ..)| p as usize)
            .expect("live handle")
    }

    /// A small block gets a SLOTTED handle (bit 62 set, bit 61 clear, low 30
    /// bits clear), is published, and every in-bounds access resolves to the
    /// same real byte the locked slow path names; the first out-of-bounds one
    /// does not.
    #[test]
    fn a_small_block_is_published_and_resolves_like_the_slow_path() {
        let _arena = super::arena_layout_test_lock();
        if !on() {
            return;
        }
        let h = unsafe_arena_allocate(64);
        assert!(unsafe_arena_addr_is_tagged(h));
        assert_eq!(h & (1 << 61), 0, "a 64-byte block is slotted");
        assert_eq!(h & ((1 << 30) - 1), 0, "a slotted base is window-aligned");
        assert!(unsafe_arena::store().jit_published(h));
        for (off, width) in [(0i64, 1u64), (0, 8), (60, 4), (56, 8), (63, 1)] {
            assert_eq!(
                unsafe_arena::store().jit_resolve(h + off, width),
                Some(real(h + off, width as usize)),
                "offset {off} width {width}"
            );
        }
        for (off, width) in [(61i64, 4u64), (57, 8), (64, 1), (1 << 20, 1)] {
            assert_eq!(
                unsafe_arena::store().jit_resolve(h + off, width),
                None,
                "offset {off} width {width} is past the block"
            );
        }
        // A write through the resolved address is the block's own byte.
        let p = unsafe_arena::store()
            .jit_resolve(h + 9, 1)
            .expect("resolves");
        // SAFETY: the block is live and 64 bytes long; offset 9 is inside it.
        unsafe { std::ptr::write(p as *mut u8, 0x5C) };
        assert_eq!(unsafe_arena_get_byte(h + 9), 0x5C);
        unsafe_arena_free(h);
    }

    /// `free` withdraws the block before it dies, so a use-after-free misses
    /// inline exactly as it misses on the slow path; and the key is never
    /// handed out again (a later block lands at a strictly greater handle).
    #[test]
    fn free_unpublishes_and_a_key_never_returns() {
        let _arena = super::arena_layout_test_lock();
        if !on() {
            return;
        }
        let h = unsafe_arena_allocate(32);
        assert!(unsafe_arena::store().jit_published(h));
        unsafe_arena_free(h);
        assert!(!unsafe_arena::store().jit_published(h));
        assert_eq!(unsafe_arena::store().jit_resolve(h, 1), None);
        assert!(!unsafe_arena_contains(h), "and the slow path agrees");
        let mut last = h;
        for _ in 0..(1 << 16) + 7 {
            // More allocations than the table has entries: entries are
            // reused, keys never.
            let n = unsafe_arena_allocate(16);
            if n & (1 << 61) == 0 {
                assert!(
                    n > last,
                    "slotted handles only grow: {n:#x} after {last:#x}"
                );
                last = n;
            }
            unsafe_arena_free(n);
        }
        assert_eq!(unsafe_arena::store().jit_resolve(h, 1), None);
    }

    /// A resize withdraws the block: in place it keeps its handle on the slow
    /// path only; a relocation publishes the new handle.
    #[test]
    fn a_resize_unpublishes_and_a_relocation_republishes() {
        let _arena = super::arena_layout_test_lock();
        if !on() {
            return;
        }
        let h = unsafe_arena_allocate(60);
        assert!(unsafe_arena_put_byte(h + 3, 0x2B));
        let same = unsafe_arena_try_reallocate(h, 64).expect("resize succeeds");
        assert_eq!(same, h);
        assert!(
            !unsafe_arena::store().jit_published(h),
            "in place: withdrawn"
        );
        assert_eq!(unsafe_arena::store().jit_resolve(h + 3, 1), None);
        assert_eq!(
            unsafe_arena_get_byte(h + 3),
            0x2B,
            "the slow path still serves it"
        );
        let moved = unsafe_arena_try_reallocate(h, 4096).expect("resize succeeds");
        assert_ne!(moved, h);
        assert!(unsafe_arena::store().jit_published(moved));
        assert_eq!(
            unsafe_arena::store().jit_resolve(moved + 3, 1),
            Some(real(moved + 3, 1))
        );
        assert_eq!(unsafe_arena_get_byte(moved + 3), 0x2B);
        unsafe_arena_free(moved);
    }

    /// A zero-length block takes a LEGACY handle (bit 61), which the table
    /// never answers for; the tag still classifies it.
    #[test]
    fn a_legacy_handle_is_never_resolved_inline() {
        let _arena = super::arena_layout_test_lock();
        let h = unsafe_arena_allocate(0);
        assert!(unsafe_arena_addr_is_tagged(h));
        assert_ne!(h & (1 << 61), 0);
        assert_eq!(unsafe_arena::store().jit_resolve(h, 0), None);
        unsafe_arena_free(h);
    }

    /// The published layout constants are the struct's.
    #[test]
    fn the_published_layout_is_the_table_entry() {
        assert_eq!(UNSAFE_ARENA_JIT_SLOT_SHIFT, 30);
        assert_eq!(UNSAFE_ARENA_JIT_INDEX_BITS, 15);
        assert_eq!(UNSAFE_ARENA_JIT_ENTRY_SHIFT, 5);
        assert_eq!(
            (
                UNSAFE_ARENA_JIT_KEY_OFFSET,
                UNSAFE_ARENA_JIT_LEN_OFFSET,
                UNSAFE_ARENA_JIT_PTR_OFFSET
            ),
            (0, 8, 16)
        );
        if on() {
            assert_eq!(
                unsafe_arena_jit_table_addr() % 32,
                0,
                "entries are 32-aligned"
            );
        }
    }
}

#[cfg(test)]
mod unsafe_static_field_offset_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::FieldMetadata;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn static_field_offset_uses_static_storage_not_class_mirror_slots() {
        let mut ctx = MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized("sun/misc/Unsafe")
            .expect("mock class id");
        let meta = FieldMetadata {
            name: "memoryAccessWarned".to_string(),
            descriptor: "Z".to_string(),
            access_flags: 0x0008,
            slot_index: 24,
            declaring_class_id: class_id,
            is_static: true,
        };
        ctx.set_static_field(class_id, 24, Value::Int(0));

        let field = crate::lang_class::create_field_object(&mut ctx, &meta);
        let offset = match native_unsafe_static_field_offset(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(field))],
        )
        .expect("staticFieldOffset")
        .expect("offset value")
        {
            Value::Long(offset) => offset as usize,
            other => panic!("unexpected offset value: {other:?}"),
        };

        assert!(is_synthetic_offset(offset));
        assert_ne!(offset, 24);

        let base = ctx.get_class_mirror(class_id);
        ctx.set_field(base, 24, Value::Int(77));

        let before = native_unsafe_get_int_volatile(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(base)),
                Value::Long(offset as i64),
            ],
        )
        .expect("getBooleanVolatile")
        .expect("read value");
        assert_eq!(before, Value::Int(0));

        let cas = native_unsafe_cas_int(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(base)),
                Value::Long(offset as i64),
                Value::Int(0),
                Value::Int(1),
            ],
        )
        .expect("compareAndSetBoolean")
        .expect("cas value");
        assert_eq!(cas, Value::Int(1));
        assert_eq!(ctx.get_static_field(class_id, 24), Value::Int(1));
        assert_eq!(ctx.get_field(base, 24), Value::Int(77));
    }

    #[test]
    fn static_long_null_base_unsafe_uses_real_static_storage() {
        let mut ctx = MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized("java/lang/Thread$ThreadIdentifiers")
            .expect("mock class id");
        let meta = FieldMetadata {
            name: "next".to_string(),
            descriptor: "J".to_string(),
            access_flags: 0x0008,
            slot_index: 7,
            declaring_class_id: class_id,
            is_static: true,
        };
        ctx.set_static_field(class_id, 7, Value::Long(42));

        let field = crate::lang_class::create_field_object(&mut ctx, &meta);
        let offset = match native_unsafe_static_field_offset(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(field))],
        )
        .expect("staticFieldOffset")
        .expect("offset value")
        {
            Value::Long(offset) => offset as usize,
            other => panic!("unexpected offset value: {other:?}"),
        };

        let before = native_unsafe_get_long_volatile(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
            ],
        )
        .expect("getLongVolatile")
        .expect("read value");
        assert_eq!(before, Value::Long(42));

        let old = native_unsafe_get_and_add_long(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
                Value::Long(1),
            ],
        )
        .expect("getAndAddLong")
        .expect("old value");

        assert_eq!(old, Value::Long(42));
        assert_eq!(ctx.get_static_field(class_id, 7), Value::Long(43));
    }

    #[test]
    fn thread_next_tid_offset_seeds_positive_null_base_counter() {
        let mut ctx = MockNativeContext::new();
        // A private VM identity: the counter is per VM since gc-common w8-a,
        // and mock contexts otherwise all share identity 0.
        let vm = 0x7A80_0101;
        ctx.set_vm_identity(vm);
        let offset = thread_next_tid_offset(vm);
        // `&mut ctx`: this test OWNS its MockNativeContext, unlike the 21
        // production call sites which already hold a `&mut dyn NativeContext`.
        let _ = note_unsafe_side_store_offset(&mut ctx, offset, line!(), 3);
        lock_static_long_shard(offset).insert((vm, offset), 1);

        let first = native_unsafe_get_and_add_long(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
                Value::Long(1),
            ],
        )
        .expect("first getAndAddLong")
        .expect("first old value");
        let second = native_unsafe_get_and_add_long(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
                Value::Long(1),
            ],
        )
        .expect("second getAndAddLong")
        .expect("second old value");

        assert_ne!(offset, 0);
        assert_eq!(first, Value::Long(1));
        assert_eq!(second, Value::Long(2));
        crate::forget_vm_unsafe_side_stores(vm);
    }

    /// Same shape as `static_long_null_base_unsafe_uses_real_static_storage`
    /// above, for an int static — the null-base int accessors previously
    /// serviced `static_int_store` unconditionally, invisible to putstatic.
    #[test]
    fn static_int_null_base_unsafe_uses_real_static_storage() {
        let mut ctx = MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized("com/example/IntStaticHolder")
            .expect("mock class id");
        let meta = FieldMetadata {
            name: "counter".to_string(),
            descriptor: "I".to_string(),
            access_flags: 0x0008,
            slot_index: 0,
            declaring_class_id: class_id,
            is_static: true,
        };
        ctx.set_static_field(class_id, 0, Value::Int(10));

        let field = crate::lang_class::create_field_object(&mut ctx, &meta);
        let offset = match native_unsafe_static_field_offset(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(field))],
        )
        .expect("staticFieldOffset")
        .expect("offset value")
        {
            Value::Long(offset) => offset as usize,
            other => panic!("unexpected offset value: {other:?}"),
        };

        let before = native_unsafe_get_int_volatile(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
            ],
        )
        .expect("getIntVolatile")
        .expect("read value");
        assert_eq!(before, Value::Int(10));

        let old = native_unsafe_get_and_add_int(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
                Value::Int(1),
            ],
        )
        .expect("getAndAddInt")
        .expect("old value");
        assert_eq!(old, Value::Int(10));
        assert_eq!(ctx.get_static_field(class_id, 0), Value::Int(11));

        let cas_ok = native_unsafe_cas_int(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
                Value::Int(11),
                Value::Int(99),
            ],
        )
        .expect("compareAndSetInt")
        .expect("cas result");
        assert_eq!(cas_ok, Value::Int(1));
        assert_eq!(ctx.get_static_field(class_id, 0), Value::Int(99));
    }

    /// Same shape, for a static Object field.
    #[test]
    fn static_object_null_base_unsafe_uses_real_static_storage() {
        let mut ctx = MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized("com/example/ObjectStaticHolder")
            .expect("mock class id");
        let meta = FieldMetadata {
            name: "value".to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0x0008,
            slot_index: 0,
            declaring_class_id: class_id,
            is_static: true,
        };
        let seeded = ctx.create_string("seed");
        ctx.set_static_field(class_id, 0, Value::Object(Some(seeded)));

        let field = crate::lang_class::create_field_object(&mut ctx, &meta);
        let offset = match native_unsafe_static_field_offset(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(field))],
        )
        .expect("staticFieldOffset")
        .expect("offset value")
        {
            Value::Long(offset) => offset as usize,
            other => panic!("unexpected offset value: {other:?}"),
        };

        let before = native_unsafe_get_object_volatile(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
            ],
        )
        .expect("getObjectVolatile")
        .expect("read value");
        assert_eq!(before, Value::Object(Some(seeded)));

        let replacement = ctx.create_string("replacement");
        let prev = native_unsafe_get_and_set_object(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
                Value::Object(Some(replacement)),
            ],
        )
        .expect("getAndSetObject")
        .expect("previous value");
        assert_eq!(prev, Value::Object(Some(seeded)));
        assert_eq!(
            ctx.get_static_field(class_id, 0),
            Value::Object(Some(replacement))
        );
    }
}

#[cfg(test)]
mod arena_zero_length_copy_tests {
    use super::{unsafe_arena_allocate, unsafe_arena_copy_in, unsafe_arena_copy_out};

    /// A zero-length copy is a no-op and must succeed at ANY offset in the
    /// block, INCLUDING one past the last byte -- that is where a direct
    /// `ByteBuffer` sitting at its own limit points. Before the fix,
    /// `locate`'s exclusive range test rejected `base + size` and the
    /// ByteBuffer natives raised `IllegalStateException: ByteBuffer.put:
    /// direct destination write failed`.
    #[test]
    fn zero_length_copy_at_end_of_block_succeeds() {
        let _arena = super::arena_layout_test_lock();
        let base = unsafe_arena_allocate(16);
        assert!(base > 0, "arena allocate returned {base}");
        let end = base + 16;
        assert!(
            unsafe_arena_copy_in(end, &[]),
            "0-byte write one past the end must succeed"
        );
        let mut empty: [u8; 0] = [];
        assert!(
            unsafe_arena_copy_out(end, &mut empty),
            "0-byte read one past the end must succeed"
        );
        // Interior offsets keep working, and a NON-empty copy past the end is
        // still refused.
        assert!(unsafe_arena_copy_in(base + 8, &[1, 2, 3, 4]));
        let mut got = [0u8; 4];
        assert!(unsafe_arena_copy_out(base + 8, &mut got));
        assert_eq!(got, [1, 2, 3, 4]);
        assert!(
            !unsafe_arena_copy_in(end, &[9]),
            "a 1-byte write past the end must still be refused"
        );
    }
}

#[cfg(test)]
mod unsafe_unaligned_read_tests {
    use super::{unsafe_read_bytes_into, UNSAFE_MAX_ACCESS_WIDTH};
    use cratonvm_native_api::test_mock::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;
    use cratonvm_types::ArrayElementType as Aet;

    /// `unsafe_read_bytes_from_array`'s ABASE, restated so a test failure names
    /// the offset convention rather than a bare number.
    const ABASE: usize = 16;

    /// **The `byte[]` bulk route and the per-element loop must answer
    /// identically, at every width and every alignment.**
    ///
    /// The bulk route is the one `HeapByteBuffer.getInt`/`getLong` take, and it
    /// is selected by element size rather than by anything the caller says — so
    /// nothing at the call site would notice if it disagreed with the loop it
    /// replaced. `short[]` is the control: same bytes, same expected answer,
    /// but two-byte elements, so it can only reach the generic loop.
    #[test]
    fn the_byte_array_bulk_route_agrees_with_the_per_element_loop() {
        let mut ctx = MockNativeContext::new();
        let bytes = ctx.new_array(Aet::Byte, 64);
        let shorts = ctx.new_array(Aet::Short, 32);
        // `i * 7 + 1` as a BYTE pattern, wrapping. Written inline as
        // `i as u8 * 7 + 1` this panics in a debug build from i = 37, where
        // 37 * 7 = 259 leaves `u8` -- and both loops below had it, so the test
        // could never reach the widths it was written to cover. One closure so
        // the byte[] and short[] sides cannot drift apart again.
        let b = |i: usize| -> u8 { (i as u8).wrapping_mul(7).wrapping_add(1) };
        for i in 0..64usize {
            // Cast: a byte element round-trips as a sign-extended int.
            ctx.set_array_element(bytes, i, cratonvm_types::Value::Int(i32::from(b(i) as i8)));
        }
        for i in 0..32usize {
            // The same 64 bytes, little-endian, two per element.
            let lo = u16::from(b(i * 2));
            let hi = u16::from(b(i * 2 + 1));
            ctx.set_array_element(
                shorts,
                i,
                cratonvm_types::Value::Int(i32::from((lo | (hi << 8)) as i16)),
            );
        }
        for width in [2usize, 4, 8] {
            for off in 0..8usize {
                let mut from_bytes = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
                let mut from_shorts = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
                assert!(
                    unsafe_read_bytes_into(&ctx, bytes, ABASE + off, width, &mut from_bytes),
                    "byte[] read refused at width {width} off {off}"
                );
                assert!(
                    unsafe_read_bytes_into(&ctx, shorts, ABASE + off, width, &mut from_shorts),
                    "short[] read refused at width {width} off {off}"
                );
                assert_eq!(
                    from_bytes[..width],
                    from_shorts[..width],
                    "bulk byte[] route disagrees with the spanning loop at width {width} off {off}"
                );
            }
        }
    }

    /// **A read that runs off the end is refused, on both routes.**
    ///
    /// The fast path is entered only after the same `rel + width <=
    /// total_bytes` check the loop uses, so this pins that the check was not
    /// moved behind it. An offset below ABASE is refused too: it is not an
    /// array-element offset at all.
    #[test]
    fn an_out_of_range_read_is_refused_rather_than_truncated() {
        let mut ctx = MockNativeContext::new();
        let bytes = ctx.new_array(Aet::Byte, 8);
        let mut out = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
        assert!(unsafe_read_bytes_into(&ctx, bytes, ABASE, 8, &mut out));
        assert!(
            !unsafe_read_bytes_into(&ctx, bytes, ABASE + 1, 8, &mut out),
            "one byte past the end must be refused"
        );
        assert!(
            !unsafe_read_bytes_into(&ctx, bytes, ABASE - 1, 2, &mut out),
            "an offset below ABASE is not an element offset"
        );
        assert!(
            !unsafe_read_bytes_into(&ctx, bytes, ABASE, UNSAFE_MAX_ACCESS_WIDTH + 1, &mut out),
            "a width wider than the buffer must be refused, not truncated"
        );
    }

    /// **A reference array has no byte-addressable storage and is refused.**
    /// The fast path keys on element SIZE, so this pins that the element-type
    /// match still rejects `Reference` before any size is derived.
    #[test]
    fn a_reference_array_is_refused() {
        let mut ctx = MockNativeContext::new();
        let refs = ctx.new_array(Aet::Reference, 8);
        let mut out = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
        assert!(!unsafe_read_bytes_into(&ctx, refs, ABASE, 4, &mut out));
    }
}

/// Round nio20 wave 2, lane L4 (proposal L2-4): the int/long volatile and CAS
/// natives over a primitive array of another element type act on the raw
/// little-endian bytes, as the plain `*_mb` accessors already did.
#[cfg(test)]
mod unsafe_wide_raw_tests {
    use super::{
        native_unsafe_cas_int, native_unsafe_cas_long, native_unsafe_compare_and_exchange_long,
        native_unsafe_get_long_mb, native_unsafe_get_long_volatile_arena,
        native_unsafe_put_int_volatile_arena,
    };
    use cratonvm_native_api::test_mock::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;
    use cratonvm_types::ArrayElementType as Aet;
    use cratonvm_types::Value;

    const ABASE: i64 = 16;

    #[test]
    fn long_volatile_and_cas_over_a_byte_array_use_the_raw_bytes() {
        let mut ctx = MockNativeContext::new();
        let bytes = ctx.new_array(Aet::Byte, 16);
        for i in 0..16usize {
            ctx.set_array_element(bytes, i, Value::Int(i as i32 + 1));
        }
        let base = Value::Object(Some(bytes));
        let want = i64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]);
        let args = [Value::Object(None), base, Value::Long(ABASE)];
        assert_eq!(
            native_unsafe_get_long_volatile_arena(&mut ctx, &args).expect("getLongVolatile"),
            Some(Value::Long(want))
        );
        // The plain accessor agrees (it was already raw).
        assert_eq!(
            native_unsafe_get_long_mb(&mut ctx, &args).expect("getLong"),
            Some(Value::Long(want))
        );
        // A failing CAS leaves the bytes; a matching one swaps all eight.
        let cas = |ctx: &mut MockNativeContext, ex: i64, nv: i64| {
            native_unsafe_cas_long(
                ctx,
                &[
                    Value::Object(None),
                    base,
                    Value::Long(ABASE),
                    Value::Long(ex),
                    Value::Long(nv),
                ],
            )
            .expect("compareAndSetLong")
        };
        assert_eq!(cas(&mut ctx, 0, -1), Some(Value::Int(0)));
        assert_eq!(cas(&mut ctx, want, -1), Some(Value::Int(1)));
        for i in 0..8usize {
            assert_eq!(ctx.get_array_element(bytes, i), Value::Int(-1), "byte {i}");
        }
        assert_eq!(
            ctx.get_array_element(bytes, 8),
            Value::Int(9),
            "byte 8 untouched"
        );
        // compareAndExchange answers the witnessed raw value.
        let cae = native_unsafe_compare_and_exchange_long(
            &mut ctx,
            &[
                Value::Object(None),
                base,
                Value::Long(ABASE),
                Value::Long(0),
                Value::Long(5),
            ],
        )
        .expect("compareAndExchangeLong");
        assert_eq!(cae, Some(Value::Long(-1)));
    }

    #[test]
    fn int_cas_over_a_float_array_compares_the_bits() {
        let mut ctx = MockNativeContext::new();
        let floats = ctx.new_array(Aet::Float, 2);
        ctx.set_array_element(floats, 1, Value::Float(1.5));
        let base = Value::Object(Some(floats));
        // `compareAndSetFloat(a, base + 4, 1.5f, 2.5f)` is this call.
        let r = native_unsafe_cas_int(
            &mut ctx,
            &[
                Value::Object(None),
                base,
                Value::Long(ABASE + 4),
                Value::Int(1.5f32.to_bits() as i32),
                Value::Int(2.5f32.to_bits() as i32),
            ],
        )
        .expect("compareAndSetInt");
        assert_eq!(r, Some(Value::Int(1)));
        assert_eq!(ctx.get_array_element(floats, 1), Value::Float(2.5));
    }

    #[test]
    fn int_volatile_store_into_a_long_array_writes_one_lane() {
        let mut ctx = MockNativeContext::new();
        let longs = ctx.new_array(Aet::Long, 1);
        let base = Value::Object(Some(longs));
        native_unsafe_put_int_volatile_arena(
            &mut ctx,
            &[
                Value::Object(None),
                base,
                Value::Long(ABASE + 4),
                Value::Int(0x1234_5678),
            ],
        )
        .expect("putIntVolatile");
        assert_eq!(
            ctx.get_array_element(longs, 0),
            Value::Long(0x1234_5678_0000_0000)
        );
    }
}

#[cfg(test)]
mod allocate_memory_delegation_tests {
    use super::{
        native_unsafe_allocate_memory, native_unsafe_allocate_memory_realloc,
        unsafe_arena_contains, unsafe_arena_free,
    };
    use cratonvm_native_api::test_mock::MockNativeContext;
    use cratonvm_types::Value;

    /// Both `allocateMemory` bodies hand out ARENA addresses, never the heap
    /// address of a managed array: a heap address in a Java `long` is an
    /// unminted smuggle the GC roots but never remaps
    /// (`i1-L4-operand-stack-roots-unminted-primitive-longs`). And a block
    /// is readable by the raw accessors only if the arena knows it.
    #[test]
    fn allocate_and_reallocate_return_arena_addresses() {
        let _arena = super::arena_layout_test_lock();
        let mut ctx = MockNativeContext::new();
        let this = Value::Object(None);
        let Ok(Some(Value::Long(addr))) =
            native_unsafe_allocate_memory(&mut ctx, &[this, Value::Long(64)])
        else {
            panic!("allocateMemory(64) must return a long");
        };
        assert!(
            unsafe_arena_contains(addr),
            "0x{addr:x} is not an arena block"
        );
        let Ok(Some(Value::Long(addr2))) = native_unsafe_allocate_memory_realloc(
            &mut ctx,
            &[this, Value::Long(addr), Value::Long(128)],
        ) else {
            panic!("reallocateMemory(addr, 128) must return a long");
        };
        assert!(
            unsafe_arena_contains(addr2),
            "0x{addr2:x} is not an arena block"
        );
        unsafe_arena_free(addr2);
        // HotSpot: allocateMemory(0) returns 0 (the old body allocated a
        // one-element array and returned its address).
        assert_eq!(
            native_unsafe_allocate_memory(&mut ctx, &[this, Value::Long(0)]).ok(),
            Some(Some(Value::Long(0)))
        );
    }
}

/// gc-common w7-a (`common-w6a-unsafe-synthetic-field-store-is-shared-across-vms`):
/// the two `ObjectRef`-holding Unsafe side stores are keyed by VM, and their GC
/// hooks root, remap and forget one VM's rows only.
#[cfg(test)]
mod unsafe_side_store_vm_isolation_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{NativeClassAccess, NativeSystemAccess};

    // Identities no other test uses (mock contexts default to 0).
    const VM_A: usize = 0x7A70_0001;
    const VM_B: usize = 0x7A70_0002;

    /// The tests below share `VM_A` / `VM_B` and the process-wide per-VM
    /// stores, and several end with `forget_vm_unsafe_side_stores`, so run in
    /// parallel one test's registrations and forgets land in another's
    /// assertions (seen at the wave-8 merge: the synthetic-slot test failed
    /// in the module run and passed alone). One test at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn oref(addr: usize) -> ObjectRef {
        // Never dereferenced: the stores only compare and forward addresses,
        // and the mock's identity hash is the address itself.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn ctx_of(vm: usize) -> MockNativeContext {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(vm);
        ctx
    }

    fn roots_of(vm: usize) -> Vec<ObjectRef> {
        let mut out = Vec::new();
        gc_scan_unsafe_side_store_roots(vm, &mut out);
        out
    }

    /// The same receiver identity hash and synthetic offset in two VMs -- what
    /// two VMs minting hashes from the same counter produce -- are two slots.
    #[test]
    fn synthetic_slots_with_one_identity_hash_are_per_vm() {
        let _serial = serial();
        let offset = crate::synthetic_offset_for("w7a/SyntheticIsolation", "slot");
        assert!(crate::is_synthetic_offset(offset));
        let recv = oref(0x7A70_1000);
        let (va, vb) = (oref(0x7A70_2000), oref(0x7A70_3000));
        let mut a = ctx_of(VM_A);
        let mut b = ctx_of(VM_B);

        assert!(crate::synthetic_cas(
            &mut a,
            recv,
            offset,
            Value::Object(None),
            Value::Object(Some(va))
        ));
        assert_eq!(
            crate::synthetic_get(&mut b, recv, offset),
            Value::Object(None),
            "VM B must not read VM A's value"
        );
        let (none, some_vb) = (Value::Object(None), Value::Object(Some(vb)));
        let b_won = crate::synthetic_cas(&mut b, recv, offset, none, some_vb);
        assert!(
            b_won,
            "VM B's null -> value CAS must not compare against VM A's value"
        );
        assert_eq!(
            crate::synthetic_get(&mut a, recv, offset),
            Value::Object(Some(va))
        );

        assert!(roots_of(VM_A).contains(&va) && !roots_of(VM_A).contains(&vb));
        assert!(roots_of(VM_B).contains(&vb) && !roots_of(VM_B).contains(&va));

        // B's pointer map never rewrites A's value, even when it names it.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x7A70_2000, 0x7A70_4000);
        gc_update_unsafe_side_store_refs(VM_B, &map);
        assert_eq!(
            crate::synthetic_get(&mut a, recv, offset),
            Value::Object(Some(va))
        );
        gc_update_unsafe_side_store_refs(VM_A, &map);
        assert_eq!(
            crate::synthetic_get(&mut a, recv, offset),
            Value::Object(Some(oref(0x7A70_4000)))
        );

        crate::forget_vm_unsafe_side_stores(VM_A);
        assert_eq!(
            crate::synthetic_get(&mut a, recv, offset),
            Value::Object(None)
        );
        assert_eq!(
            crate::synthetic_get(&mut b, recv, offset),
            Value::Object(Some(vb))
        );
        crate::forget_vm_unsafe_side_stores(VM_B);
        assert!(roots_of(VM_B).is_empty());
    }

    /// The null-base object store (an Unsafe reference access with a null base
    /// at a classified offset nothing registered as a static) is per VM too.
    #[test]
    fn null_base_object_slots_are_per_vm() {
        let _serial = serial();
        let offset = crate::synthetic_offset_for("w7a/NullBaseIsolation", "static:ghost");
        let va = oref(0x7A70_5000);
        let mut a = ctx_of(VM_A + 2);
        let mut b = ctx_of(VM_B + 2);
        let get = |ctx: &mut MockNativeContext| {
            native_unsafe_get_object(
                ctx,
                &[
                    Value::Object(None),
                    Value::Object(None),
                    Value::Long(offset as i64),
                ],
            )
            .expect("getReference")
            .expect("a value")
        };

        native_unsafe_put_object(
            &mut a,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
                Value::Object(Some(va)),
            ],
        )
        .expect("putReference");
        assert_eq!(get(&mut a), Value::Object(Some(va)));
        assert_eq!(
            get(&mut b),
            Value::Object(None),
            "VM B must not read VM A's slot"
        );
        assert!(roots_of(VM_A + 2).contains(&va));
        assert!(
            !roots_of(VM_B + 2).contains(&va),
            "B's collection must not root A's value"
        );

        crate::forget_vm_unsafe_side_stores(VM_A + 2);
        assert_eq!(get(&mut a), Value::Object(None));
        crate::forget_vm_unsafe_side_stores(VM_B + 2);
    }

    fn null_base(offset: usize, rest: &[Value]) -> Vec<Value> {
        let mut args = vec![
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
        ];
        args.extend_from_slice(rest);
        args
    }

    /// Interpreter round i5 wave 103, lane L2: two classes of ONE name in ONE
    /// VM (an application class and a user loader's copy) get two static
    /// offsets, each resolving to its own class's slot. Minted by name, the
    /// second registration took the first's offset over, and the application
    /// class's reflective static accesses reached the copy.
    #[test]
    fn same_named_classes_in_one_vm_get_their_own_static_offsets() {
        let _serial = serial();
        let vm = VM_A + 9;
        let (app, copy) = (ClassId::new(595), ClassId::new(1203));
        let name = "i103/SameNamedShape";
        let off_app = crate::register_static_field_offset_in_vm(vm, name, "latched", app, 0);
        let off_copy = crate::register_static_field_offset_in_vm(vm, name, "latched", copy, 0);
        assert_ne!(off_app, off_copy);
        let ctx = ctx_of(vm);
        assert_eq!(
            unsafe_static_field_target(&ctx, off_app as usize),
            Some((app, 0))
        );
        assert_eq!(
            unsafe_static_field_target(&ctx, off_copy as usize),
            Some((copy, 0))
        );
        crate::forget_vm_unsafe_side_stores(vm);
    }

    /// gc-common w8-a: one `(class, field)` name is one offset in every VM, and
    /// two VMs that registered it under DIFFERENT class ids each reach their
    /// own static through a null-base access -- never the class the other VM's
    /// id happens to name here.
    #[test]
    fn static_field_offsets_resolve_in_the_calling_vm() {
        let _serial = serial();
        let (vm_a, vm_b) = (VM_A + 4, VM_B + 4);
        let (foo_in_a, foo_in_b) = (ClassId::new(700), ClassId::new(912));
        let name = "w8a/StaticTargetIsolation";
        let off_a = crate::register_static_field_offset_in_vm(vm_a, name, "counter", foo_in_a, 3);
        // Interpreter round i5 wave 103, lane L2: offsets are minted per
        // (name, class id) now, so the two registrations differ; the same id
        // in both VMs would share one. The isolation this test is about is the
        // per-VM table, so VM B registers its class under VM A's offset.
        let off_b = crate::register_static_field_offset_in_vm(vm_b, name, "counter", foo_in_b, 3);
        assert_ne!(off_a, off_b, "two class ids of one name get two offsets");
        assert_eq!(
            crate::register_static_field_offset_in_vm(vm_b, name, "counter", foo_in_a, 3),
            off_a,
            "one (name, id) is one offset in every VM"
        );
        let offset = off_a as usize;
        remember_unsafe_static_field_offset(vm_b, offset, foo_in_b, 3);
        let mut a = ctx_of(vm_a);
        let mut b = ctx_of(vm_b);
        a.set_static_field(foo_in_a, 3, Value::Long(10));
        // What VM B's class id names in VM A: some unrelated class's static.
        a.set_static_field(foo_in_b, 3, Value::Long(99));
        b.set_static_field(foo_in_b, 3, Value::Long(20));
        let add = |ctx: &mut MockNativeContext| {
            native_unsafe_get_and_add_long(ctx, &null_base(offset, &[Value::Long(1)]))
                .expect("getAndAddLong")
                .expect("old value")
        };

        assert_eq!(add(&mut a), Value::Long(10));
        assert_eq!(add(&mut b), Value::Long(20));
        assert_eq!(a.get_static_field(foo_in_a, 3), Value::Long(11));
        assert_eq!(
            a.get_static_field(foo_in_b, 3),
            Value::Long(99),
            "VM B's class id must never be dereferenced in VM A"
        );
        assert_eq!(b.get_static_field(foo_in_b, 3), Value::Long(21));

        // Teardown drops one VM's registration only.
        crate::forget_vm_unsafe_side_stores(vm_a);
        assert!(unsafe_static_field_target(&a, offset).is_none());
        assert_eq!(unsafe_static_field_target(&b, offset), Some((foo_in_b, 3)));
        crate::forget_vm_unsafe_side_stores(vm_b);
    }

    /// The null-base `int` / `long` fallback stores (a classified offset no VM
    /// registered as a static) are per VM, and so is the tid counter behind
    /// `Thread$ThreadIdentifiers`.
    #[test]
    fn null_base_primitive_slots_and_the_tid_counter_are_per_vm() {
        let _serial = serial();
        let (vm_a, vm_b) = (VM_A + 6, VM_B + 6);
        let offset = crate::synthetic_offset_for("w8a/NullBasePrimitive", "native:slot");
        assert!(crate::is_synthetic_offset(offset));
        let mut a = ctx_of(vm_a);
        let mut b = ctx_of(vm_b);

        native_unsafe_put_int(&mut a, &null_base(offset, &[Value::Int(41)])).expect("putInt");
        let get_int = |ctx: &mut MockNativeContext| {
            native_unsafe_get_int(ctx, &null_base(offset, &[]))
                .expect("getInt")
                .expect("a value")
        };
        assert_eq!(get_int(&mut a), Value::Int(41));
        assert_eq!(
            get_int(&mut b),
            Value::Int(0),
            "VM B must not read VM A's slot"
        );
        let cas =
            native_unsafe_cas_int(&mut b, &null_base(offset, &[Value::Int(0), Value::Int(5)]))
                .expect("compareAndSetInt");
        assert_eq!(
            cas,
            Some(Value::Int(1)),
            "B's 0 -> 5 must not compare against A's 41"
        );
        assert_eq!(get_int(&mut a), Value::Int(41));
        assert_eq!(get_int(&mut b), Value::Int(5));

        let long_offset = crate::synthetic_offset_for("w8a/NullBasePrimitive", "native:long");
        let add = |ctx: &mut MockNativeContext| {
            native_unsafe_get_and_add_long(ctx, &null_base(long_offset, &[Value::Long(1)]))
                .expect("getAndAddLong")
                .expect("old value")
        };
        assert_eq!(add(&mut a), Value::Long(0));
        assert_eq!(add(&mut a), Value::Long(1));
        assert_eq!(
            add(&mut b),
            Value::Long(0),
            "B's counter starts at its own seed"
        );

        // Each VM's first application thread gets tid 4 (JDK 25:
        // `Thread.PRIMORDIAL_TID` 3 is `main`'s, the counter starts past it).
        assert_eq!(crate::next_java_thread_tid(vm_a), 4);
        assert_eq!(crate::next_java_thread_tid(vm_a), 5);
        assert_eq!(crate::next_java_thread_tid(vm_b), 4);
        // A VM-built mirror (`mint_vm_thread_java_tid`) draws from the same
        // counter, so it can never collide with a Java-constructed thread.
        assert_eq!(crate::mint_vm_thread_java_tid(vm_a), 6);
        assert_eq!(crate::next_java_thread_tid(vm_a), 7);

        crate::forget_vm_unsafe_side_stores(vm_a);
        assert_eq!(get_int(&mut a), Value::Int(0));
        assert_eq!(
            crate::next_java_thread_tid(vm_a),
            4,
            "teardown drops A's counter"
        );
        assert_eq!(get_int(&mut b), Value::Int(5));
        crate::forget_vm_unsafe_side_stores(vm_a);
        crate::forget_vm_unsafe_side_stores(vm_b);
    }

    /// The VM-less `register_static_field_offset` (kept until its one caller in
    /// `vm_util.rs` passes a VM) records the class NAME, and a VM accepts the
    /// registration only for a class of that name in its own VM.
    #[test]
    fn a_vm_less_static_registration_is_accepted_by_class_name_only() {
        let _serial = serial();
        let (vm_a, vm_b) = (VM_A + 8, VM_B + 8);
        let mut a = ctx_of(vm_a);
        let mut b = ctx_of(vm_b);
        let holder = a
            .ensure_class_initialized("w8a/UnscopedHolder")
            .expect("mock class id");
        a.set_static_field(holder, 1, Value::Int(7));
        // A different class that B may well have minted under the same id.
        let other = b
            .ensure_class_initialized("w8a/NotTheUnscopedHolder")
            .expect("mock class id");
        b.set_static_field(other, 1, Value::Int(55));
        let offset =
            crate::register_static_field_offset("w8a/UnscopedHolder", "latch", holder, 1) as usize;

        assert_eq!(unsafe_static_field_target(&a, offset), Some((holder, 1)));
        let get_int = |ctx: &mut MockNativeContext| {
            native_unsafe_get_int(ctx, &null_base(offset, &[]))
                .expect("getInt")
                .expect("a value")
        };
        assert_eq!(get_int(&mut a), Value::Int(7));
        if other == holder {
            assert_eq!(unsafe_static_field_target(&b, offset), None);
        }
        assert_ne!(
            get_int(&mut b),
            Value::Int(55),
            "B must not reach another class's static"
        );

        crate::forget_vm_unsafe_side_stores(vm_a);
        crate::forget_vm_unsafe_side_stores(vm_b);
    }
}

/// nio21 wave 7 L3 (`docs/internal/fixed-bugs/r21-unsafe-natives-test-synthetic-offset-before-the-array-FIXED-20261010.md`):
/// the arm order. A 600 MiB array is impractical, so the witness is the order
/// itself: a band offset against an ARRAY base goes to the array arm (where a
/// small array's out-of-range answer is "no heap touch"), never the side store.
#[cfg(test)]
mod nio21_w7_array_before_synthetic_tests {
    use super::{
        native_unsafe_cas_int, native_unsafe_get_int_volatile,
        unsafe_array_before_synthetic_enabled, unsafe_synthetic_field_offset,
    };
    use cratonvm_native_api::test_mock::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;
    use cratonvm_types::{ArrayElementType as Aet, ClassId, Value};

    #[test]
    fn nio21_w7_an_array_base_never_takes_the_synthetic_band() {
        if !unsafe_array_before_synthetic_enabled() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let ints = ctx.new_array(Aet::Int, 4);
        let plain = ctx.alloc_object(ClassId::new(0), 2);
        let off = crate::SYNTHETIC_OFFSET_BASE + 0x40;
        assert!(
            !unsafe_synthetic_field_offset(&ctx, ints, off),
            "an array base is never synthetic"
        );
        assert!(
            unsafe_synthetic_field_offset(&ctx, plain, off),
            "a plain base keeps the band"
        );
        assert!(
            !unsafe_synthetic_field_offset(&ctx, plain, 3),
            "below the band"
        );
        let base = Value::Object(Some(ints));
        let cas = native_unsafe_cas_int(
            &mut ctx,
            &[
                Value::Object(None),
                base,
                Value::Long(off as i64),
                Value::Int(0),
                Value::Int(7),
            ],
        )
        .expect("compareAndSetInt");
        assert_eq!(
            cas,
            Some(Value::Int(0)),
            "out of range on the array arm: the CAS fails"
        );
        for i in 0..4 {
            assert_eq!(
                ctx.get_array_element(ints, i),
                Value::Int(0),
                "element {i} untouched"
            );
        }
        assert!(
            !matches!(crate::synthetic_get(&mut ctx, ints, off), Value::Int(7)),
            "nothing was written to a side store keyed by the array"
        );
        let get = native_unsafe_get_int_volatile(
            &mut ctx,
            &[Value::Object(None), base, Value::Long(off as i64)],
        )
        .expect("getIntVolatile");
        assert_eq!(get, Some(Value::Int(0)));
    }

    #[test]
    fn nio21_w7_the_kill_switch_restores_the_band_first_order() {
        let mut ctx = MockNativeContext::new();
        let ints = ctx.new_array(Aet::Int, 4);
        let off = crate::SYNTHETIC_OFFSET_BASE + 0x40;
        let synthetic = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC", Some("0"))],
            || unsafe_synthetic_field_offset(&ctx, ints, off),
        );
        assert!(synthetic);
    }
}

/// gci i17/vt (`CRATONVM_VT_PINNED_PARK_COMPENSATE`, opt-in): the live
/// `Unsafe.park(ZJ)V` compensates the scheduler for a pinned park of a mounted
/// JDK virtual thread, and only for one.
#[cfg(test)]
mod gci_i17vt_pinned_park_compensation_tests {
    use super::native_unsafe_park;
    use crate::test_utils::MockNativeContext;
    use cratonvm_types::error::MethodCallResult;
    use cratonvm_types::Value;

    const SRC: &str = include_str!("unsafe_natives_ext.rs");

    /// The body of the first `fn <name>(` in this file (the definition precedes
    /// the tests that name it), up to its closing line.
    fn body(name: &str) -> &'static str {
        let start = SRC.find(name).expect("the function is in this file");
        let len = SRC[start..].find("\n\u{7d}\n").expect("the function ends");
        &SRC[start..start + len]
    }

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle).unwrap_or_else(|| panic!("missing: {needle}"))
    }

    fn no_java(
        _ctx: &mut MockNativeContext,
        class: &str,
        name: &str,
        _desc: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        panic!("a platform thread's park ran Java: {class}.{name}");
    }

    /// The order inside the native: the yield attempt, then the carrier test,
    /// the switch and `Blocker.begin`, the park, and `Blocker.end` only when
    /// compensation was attempted.
    #[test]
    fn gci_i17vt_the_live_park_compensates_around_the_park_only() {
        let park = body("fn native_unsafe_park(");
        let yielded = at(park, "try_yield_virtual_park(ctx, timeout)");
        let carrier = at(park, "ctx.blocks_jdk_carrier()");
        let switch = at(park, "runtime_flag_on(\"CRATONVM_VT_PINNED_PARK_COMPENSATE\")");
        let begin = at(park, "ctx.begin_carrier_compensation()");
        let door = at(park, "unsafe_park_door(ctx, timeout, is_absolute, time);\n    if compensated");
        let end = at(park, "ctx.end_carrier_compensation(true)");
        assert!(yielded < carrier, "a park that yields never compensates");
        assert!(carrier < switch && switch < begin, "the carrier test and the switch gate Blocker.begin");
        assert!(begin < door && door < end, "Blocker.end follows the park");
    }

    /// A platform thread (the mock: no JDK carrier) parks without running any
    /// Java, with the switch on.
    #[test]
    fn gci_i17vt_a_platform_park_runs_no_java_with_the_switch_on() {
        let mut ctx = MockNativeContext::new();
        ctx.set_invoke_hook(no_java);
        let answer = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_VT_PINNED_PARK_COMPENSATE", Some("1"))],
            || native_unsafe_park(&mut ctx, &[Value::Object(None), Value::Int(0), Value::Long(1)]),
        );
        assert_eq!(answer.ok(), Some(None));
    }
}

/// gci i21/qh: `Unsafe.park`'s timeout arithmetic against HotSpot 25's
/// `Parker::park` (`unsafe_park_timeout`).
#[cfg(test)]
mod gci_i21qh_park_timeout_tests {
    use super::unsafe_park_timeout;
    use std::time::Duration;

    const NOW: i64 = 1_760_000_000_000; // an epoch millisecond in 2025

    #[test]
    fn gci_i21qh_an_absolute_zero_returns_at_once_not_never() {
        assert_eq!(unsafe_park_timeout(true, 0, NOW), Some(Duration::ZERO));
    }

    #[test]
    fn gci_i21qh_a_relative_zero_is_untimed() {
        assert_eq!(unsafe_park_timeout(false, 0, NOW), None);
    }

    #[test]
    fn gci_i21qh_negative_times_return_at_once_in_both_forms() {
        assert_eq!(unsafe_park_timeout(false, -1, NOW), Some(Duration::ZERO));
        assert_eq!(unsafe_park_timeout(true, -1, NOW), Some(Duration::ZERO));
        // No overflow (a debug panic before i21).
        assert_eq!(unsafe_park_timeout(true, i64::MIN, NOW), Some(Duration::ZERO));
        assert_eq!(unsafe_park_timeout(false, i64::MIN, NOW), Some(Duration::ZERO));
    }

    #[test]
    fn gci_i21qh_absolute_deadlines_wait_out_the_rest() {
        assert_eq!(
            unsafe_park_timeout(true, NOW + 250, NOW),
            Some(Duration::from_millis(250))
        );
        assert_eq!(unsafe_park_timeout(true, NOW - 5, NOW), Some(Duration::ZERO));
        assert_eq!(unsafe_park_timeout(true, NOW, NOW), Some(Duration::ZERO));
        // A far deadline saturates instead of wrapping.
        assert_eq!(
            unsafe_park_timeout(true, i64::MAX, -1),
            Some(Duration::from_millis(i64::MAX as u64))
        );
    }

    #[test]
    fn gci_i21qh_relative_nanos_are_kept() {
        assert_eq!(
            unsafe_park_timeout(false, 50_000, NOW),
            Some(Duration::from_nanos(50_000))
        );
    }

    /// Every absolute park takes the absolute door (an absolute 0 included).
    #[test]
    fn gci_i21qh_every_absolute_park_takes_the_until_door() {
        const SRC: &str = include_str!("unsafe_natives_ext.rs");
        let start = SRC.find("fn unsafe_park_door(").expect("the door");
        let len = SRC[start..].find("\n\u{7d}\n").expect("its end");
        let door = &SRC[start..start + len];
        assert!(door.contains("    if is_absolute \u{7b}\n"), "{door}");
        assert!(!door.contains("time != 0"), "{door}");
    }
}

/// gci i22/rv (`gci-i22rv-i21-default-path-review-findings-FIXED-20261016`
/// findings 3-4): an elapsed deadline never takes the virtual-thread yield
/// (whose zero wake-up means "untimed"), and an absolute `0` posts HotSpot's
/// untimed event fields.
#[cfg(test)]
mod gci_i22rv_park_tests {
    use super::{unsafe_park_absolute_until_field, unsafe_park_returns_at_once, unsafe_park_timeout};

    const NOW: i64 = 1_760_000_000_000;
    const SRC: &str = include_str!("unsafe_natives_ext.rs");

    fn body(sig: &str) -> &'static str {
        let start = SRC.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let len = SRC[start..].find("\n\u{7d}\n").expect("its end");
        &SRC[start..start + len]
    }

    /// Every argument pair `Parker::park` returns at once for is answered
    /// "at once" here; an untimed or a pending timed park is not.
    #[test]
    fn gci_i22rv_the_at_once_answers_are_exactly_hotspots_immediate_returns() {
        for (abs, time) in [
            (false, -1),
            (false, i64::MIN),
            (true, 0),
            (true, -1),
            (true, i64::MIN),
            (true, NOW - 5),
            (true, NOW),
        ] {
            assert!(
                unsafe_park_returns_at_once(unsafe_park_timeout(abs, time, NOW)),
                "abs={abs} time={time}"
            );
        }
        for (abs, time) in [(false, 0), (false, 1), (false, 50_000), (true, NOW + 250)] {
            assert!(
                !unsafe_park_returns_at_once(unsafe_park_timeout(abs, time, NOW)),
                "abs={abs} time={time}"
            );
        }
    }

    /// The immediate return is decided with the interrupt test, before the
    /// virtual-thread yield: `try_yield_virtual_park` turns `Some(ZERO)` into
    /// `wake_after_nanos: 0`, the untimed yield.
    #[test]
    fn gci_i22rv_an_elapsed_deadline_never_reaches_the_virtual_yield() {
        let park = body("fn native_unsafe_park(");
        let gate = park
            .find("if ctx.is_interrupted(false) || unsafe_park_returns_at_once(timeout) \u{7b}")
            .expect("the at-once gate");
        let door = park[gate..]
            .find("unsafe_park_door(ctx, timeout, is_absolute, time);")
            .map(|i| gate + i)
            .expect("the gate's door");
        let yielded = park
            .find("try_yield_virtual_park(ctx, timeout)")
            .expect("the yield");
        assert!(gate < door && door < yielded, "{gate} {door} {yielded}");
        // The `LockSupport.parkUntil(Object, long)` stand-in (`--compatible`
        // with `CRATONVM_COMPAT_JDK_LOCK_SUPPORT=0`, or the synthetic library):
        // the same rule, by its remaining milliseconds.
        const LS: &str = include_str!("util_concurrent_ext.rs");
        let start = LS
            .find("fn native_lock_support_park_until(")
            .expect("the stand-in");
        let len = LS[start..].find("\n\u{7d}\n").expect("its end");
        let until = &LS[start..start + len];
        let guard = until.find("if remaining > 0 \u{7b}").expect("the guard");
        let yield_at = until
            .find("try_yield_virtual_park(ctx, timeout)")
            .expect("the yield");
        assert!(guard < yield_at);
    }

    /// `Unsafe_Park` posts `time == 0` as an untimed park (`until =
    /// Long.MIN_VALUE`) in both forms; any other absolute time as `until`.
    #[test]
    fn gci_i22rv_an_absolute_zero_posts_an_untimed_park_event() {
        assert_eq!(unsafe_park_absolute_until_field(0), i64::MIN);
        assert_eq!(unsafe_park_absolute_until_field(-1), -1);
        assert_eq!(unsafe_park_absolute_until_field(NOW), NOW);
        assert_eq!(unsafe_park_absolute_until_field(i64::MIN), i64::MIN);
        let door = body("fn unsafe_park_door(");
        assert!(
            door.contains("ctx.park_until_reporting(timeout, unsafe_park_absolute_until_field(time));"),
            "{door}"
        );
    }
}

/// gci i23/pk (`gci-i22rv-i21-default-path-review-findings-FIXED-20261016`
/// finding 6): a negative relative park carries its raw time to the
/// `jdk.ThreadPark` event (finding 5's pin is in `vm`'s
/// `virtual_threads.rs` tests: the change is the VM's park).
#[cfg(test)]
mod gci_i23pk_park_tests {
    use super::unsafe_park_relative_raw_timeout_field;

    const SRC: &str = include_str!("unsafe_natives_ext.rs");

    fn body_of(src: &'static str, sig: &str) -> &'static str {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let len = src[start..].find("\n\u{7d}\n").expect("its end");
        &src[start..start + len]
    }

    #[test]
    fn gci_i23pk_only_a_negative_relative_time_is_carried_raw() {
        assert_eq!(unsafe_park_relative_raw_timeout_field(-1), Some(-1));
        assert_eq!(
            unsafe_park_relative_raw_timeout_field(i64::MIN),
            Some(i64::MIN)
        );
        assert_eq!(unsafe_park_relative_raw_timeout_field(0), None);
        assert_eq!(unsafe_park_relative_raw_timeout_field(1), None);
        assert_eq!(unsafe_park_relative_raw_timeout_field(i64::MAX), None);
        let door = body_of(SRC, "fn unsafe_park_door(");
        let abs = door.find("if is_absolute \u{7b}").expect("the absolute arm");
        let raw = door
            .find("ctx.park_relative_reporting_raw(timeout, raw);")
            .expect("the raw arm");
        let plain = door
            .find("ctx.park_reporting_timeout(timeout);")
            .expect("the plain arm");
        assert!(abs < raw && raw < plain, "{door}");
    }
}

/// JIT round 20 wave 8c (lane huntDisp): byte field offsets
/// (`CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`), both arms of the switch.
#[cfg(test)]
mod byte_field_offset_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ClassId;

    fn obj_with(ctx: &mut MockNativeContext, values: &[Value]) -> ObjectRef {
        let obj = ctx.alloc_object(ClassId::new(1), values.len());
        for (i, v) in values.iter().enumerate() {
            ctx.set_field(obj, i, *v);
        }
        obj
    }

    fn args(obj: ObjectRef, offset: usize, rest: &[Value]) -> Vec<Value> {
        let mut a = vec![
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::Long(offset as i64),
        ];
        a.extend_from_slice(rest);
        a
    }

    #[test]
    fn encode_and_decode_round_trip_in_both_arms() {
        for slot in [0usize, 1, 2, 7, 1000] {
            assert_eq!(unsafe_encode_field_offset(false, slot), slot);
            assert_eq!(unsafe_decode_field_offset(false, slot), (slot, 0));
            let off = unsafe_encode_field_offset(true, slot);
            assert_eq!(off, UNSAFE_BYTE_FIELD_OFFSET_BASE + slot * 8);
            for k in 0..8 {
                assert_eq!(unsafe_decode_field_offset(true, off + k), (slot, k));
            }
        }
        // Below the base names no field: never slot 0.
        assert_ne!(unsafe_decode_field_offset(true, 3).0, 0);
        // The off arm is the identity for every offset.
        for off in [0usize, 1, 9, 17, 0x2000_0000] {
            assert_eq!(unsafe_decode_field_offset(false, off), (off, 0));
        }
    }

    #[test]
    fn the_off_arm_keeps_slot_offsets() {
        let mut ctx = MockNativeContext::new();
        let obj = obj_with(&mut ctx, &[Value::Int(5), Value::Int(6), Value::Int(7)]);
        assert_eq!(unsafe_field_slot(&ctx, obj, 1), (1, 0));
        assert_eq!(
            native_unsafe_get_int(&mut ctx, &args(obj, 1, &[])).expect("getInt"),
            Some(Value::Int(6))
        );
        native_unsafe_put_int(&mut ctx, &args(obj, 2, &[Value::Int(9)])).expect("putInt");
        assert_eq!(ctx.get_field(obj, 2), Value::Int(9));
        assert_eq!(ctx.get_field(obj, 1), Value::Int(6));
    }

    #[test]
    fn the_on_arm_addresses_the_bytes_inside_a_field() {
        let mut ctx = MockNativeContext::new();
        ctx.unsafe_byte_field_offsets_flag.set(true);
        let obj = obj_with(
            &mut ctx,
            &[Value::Int(0x1122_3344), Value::Long(0x0102_0304_0506_0708)],
        );
        let off_i = unsafe_encode_field_offset(true, 0);
        let off_l = unsafe_encode_field_offset(true, 1);
        assert_eq!(unsafe_field_slot(&ctx, obj, off_l + 4), (1, 4));
        // Whole fields.
        assert_eq!(
            native_unsafe_get_int(&mut ctx, &args(obj, off_i, &[])).expect("getInt"),
            Some(Value::Int(0x1122_3344))
        );
        assert_eq!(
            native_unsafe_get_long(&mut ctx, &args(obj, off_l, &[])).expect("getLong"),
            Some(Value::Long(0x0102_0304_0506_0708))
        );
        // Inside a field: its own bytes, little-endian (clamped to the field).
        assert_eq!(
            native_unsafe_get_int(&mut ctx, &args(obj, off_l + 4, &[])).expect("getInt +4"),
            Some(Value::Int(0x0102_0304))
        );
        assert_eq!(
            native_unsafe_get_int(&mut ctx, &args(obj, off_i + 1, &[])).expect("getInt +1"),
            Some(Value::Int(0x0011_2233))
        );
        // A store inside a field replaces only those bytes.
        native_unsafe_put_byte_mb(&mut ctx, &args(obj, off_i + 3, &[Value::Int(0x7f)]))
            .expect("putByte +3");
        assert_eq!(ctx.get_field(obj, 0), Value::Int(0x7f22_3344));
        native_unsafe_put_int(&mut ctx, &args(obj, off_l + 4, &[Value::Int(-1)]))
            .expect("putInt +4");
        assert_eq!(
            ctx.get_field(obj, 1),
            Value::Long(0xFFFF_FFFF_0506_0708_u64 as i64)
        );
        // An int CAS on the long field's low half (narrower than the field).
        assert_eq!(
            native_unsafe_cas_int(
                &mut ctx,
                &args(
                    obj,
                    off_l,
                    &[Value::Int(0x0506_0708), Value::Int(0x0A0B_0C0D)]
                )
            )
            .expect("compareAndSetInt"),
            Some(Value::Int(1))
        );
        assert_eq!(
            ctx.get_field(obj, 1),
            Value::Long(0xFFFF_FFFF_0A0B_0C0D_u64 as i64)
        );
        // The first field is untouched by the second's accesses.
        assert_eq!(ctx.get_field(obj, 0), Value::Int(0x7f22_3344));
    }

    /// Interpreter round i4 wave 95, lane L3: a narrow read INSIDE a field is
    /// typed as the access (`getShort` sign-extends, `getChar` zero-extends,
    /// `getByte` one byte), not the int-width fallback's four bytes.
    #[test]
    fn i95_narrow_reads_inside_a_field_are_narrowed() {
        let mut ctx = MockNativeContext::new();
        ctx.unsafe_byte_field_offsets_flag.set(true);
        let obj = obj_with(&mut ctx, &[Value::Long(0x11F2_3344_5566_7788)]);
        let off_l = unsafe_encode_field_offset(true, 0);
        assert_eq!(
            native_unsafe_get_short_mb(&mut ctx, &args(obj, off_l + 2, &[])).expect("getShort +2"),
            Some(Value::Int(0x5566))
        );
        assert_eq!(
            native_unsafe_get_short_mb(&mut ctx, &args(obj, off_l + 6, &[])).expect("getShort +6"),
            Some(Value::Int(0x11F2))
        );
        assert_eq!(
            native_unsafe_get_char_mb(&mut ctx, &args(obj, off_l + 5, &[])).expect("getChar +5"),
            Some(Value::Int(0xF233))
        );
        assert_eq!(
            native_unsafe_get_byte_mb(&mut ctx, &args(obj, off_l + 6, &[])).expect("getByte +6"),
            Some(Value::Int(-14))
        );
        // Past a 2-byte read's end inside the field: clamped, as a getInt.
        assert_eq!(
            native_unsafe_get_short_mb(&mut ctx, &args(obj, off_l + 7, &[])).expect("getShort +7"),
            Some(Value::Int(0x11))
        );
    }
}

/// gci cl48 (page
/// `gci-i47w-jw1-compatible-child-first-loader-statics-read-back-wrong-FIXED-20261017`):
/// the child-first copies of one class share the static offset minted for
/// their name, and each copy's static is still its own -- through its mirror
/// base, and through a handle that carries the base.
#[cfg(test)]
mod gci_i48_cl48_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Identities no other test uses (mock contexts default to 0).
    const VM: usize = 0xC148_0001;
    const NAME: &str = "gci48cl48/Holder";

    /// `n` same-named classes (the first by name, the rest as second copies),
    /// each with its static `graph` registered as `staticFieldOffset`
    /// registers it, in order. Since dev's `eab6d094dd` a static offset
    /// carries the class id, so every copy gets its own offset: returns the
    /// classes and their offsets, index for index.
    fn copies(ctx: &mut MockNativeContext, vm: usize, n: usize) -> (Vec<ClassId>, Vec<usize>) {
        let mut classes = vec![ctx.ensure_class_initialized(NAME).expect("mock class id")];
        for _ in 1..n {
            classes.push(ctx.declare_second_copy(NAME));
        }
        let offsets: Vec<usize> = classes
            .iter()
            .map(|&c| crate::register_static_field_offset_in_vm(vm, NAME, "graph", c, 0) as usize)
            .collect();
        let mut distinct = offsets.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), offsets.len(), "one copy, one offset");
        (classes, offsets)
    }

    #[test]
    fn gci_i48_cl48_each_copy_reads_back_its_own_static_through_its_mirror() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let (classes, offsets) = copies(&mut ctx, VM, 8);
        let mirrors: Vec<ObjectRef> = classes.iter().map(|&c| ctx.get_class_mirror(c)).collect();
        let values: Vec<ObjectRef> = classes
            .iter()
            .map(|_| ctx.alloc_object(ClassId::new(0), 1))
            .collect();
        let based = |base: ObjectRef, offset: usize, rest: &[Value]| {
            let mut a = vec![
                Value::Object(None),
                Value::Object(Some(base)),
                Value::Long(offset as i64),
            ];
            a.extend_from_slice(rest);
            a
        };
        for (k, &m) in mirrors.iter().enumerate() {
            native_unsafe_put_object(&mut ctx, &based(m, offsets[k], &[Value::Object(Some(values[k]))]))
                .expect("putReference");
        }
        for (k, &m) in mirrors.iter().enumerate() {
            let got = native_unsafe_get_object(&mut ctx, &based(m, offsets[k], &[]))
                .expect("getReference")
                .expect("a value");
            assert_eq!(
                got,
                Value::Object(Some(values[k])),
                "copy {k} reads its own value"
            );
            // The REAL static, which `getstatic` reads -- not a side slot.
            assert_eq!(
                ctx.get_static_field(classes[k], 0),
                Value::Object(Some(values[k])),
                "copy {k}'s static holds its own value"
            );
        }
        crate::forget_vm_unsafe_side_stores(VM);
    }

    #[test]
    fn gci_i48_cl48_a_base_names_its_copy_and_no_base_takes_the_latest() {
        let vm = VM + 1;
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(vm);
        let (classes, offsets) = copies(&mut ctx, vm, 3);
        for (k, &c) in classes.iter().enumerate() {
            let off = offsets[k];
            assert_eq!(unsafe_static_field_target(&ctx, off), Some((c, 0)));
            assert_eq!(unsafe_static_field_target_via_base(&ctx, None, off), Some((c, 0)));
            let m = ctx.get_class_mirror(c);
            assert_eq!(unsafe_static_field_target_for_base(&ctx, m, off), Some((c, 0)));
            assert_eq!(unsafe_static_field_target_via_base(&ctx, Some(m), off), Some((c, 0)));
            // Another copy's mirror never names this copy's offset.
            for &other in classes.iter().filter(|&&o| o != c) {
                let om = ctx.get_class_mirror(other);
                assert_eq!(unsafe_static_field_target_for_base(&ctx, om, off), None);
            }
        }
        // A copy registered again keeps its offset; the row keeps one entry.
        let again = crate::register_static_field_offset_in_vm(vm, NAME, "graph", classes[0], 0);
        assert_eq!(again as usize, offsets[0]);
        let rows = lock_unsafe_vm_offset_shard(unsafe_static_field_targets(), offsets[0])
            .get(&(vm, offsets[0]))
            .map(Vec::len);
        assert_eq!(rows, Some(1));
        crate::forget_vm_unsafe_side_stores(vm);
        assert_eq!(unsafe_static_field_target(&ctx, offsets[0]), None);
    }

    /// gcj j1/jt (page `gci-i48-unsafe-byte-lane-static-sub-target-ignores-the-base`):
    /// byte `k != 0` of a static (byte field offsets) resolves its containing
    /// static against the access's base mirror, as the word lane does, so two
    /// copies sharing one scaled offset each read, write and CAS their own
    /// static; no base keeps the latest registration.
    #[test]
    fn gcj1jt_the_byte_lane_of_a_static_resolves_against_its_base() {
        let vm = VM + 2;
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(vm);
        ctx.unsafe_byte_field_offsets_flag.set(true);
        let (classes, offsets) = copies(&mut ctx, vm, 2);
        let (c0, c1) = (classes[0], classes[1]);
        let scaled = unsafe_static_offset_answer(&ctx, offsets[0], c0, 0);
        assert!(scaled >= UNSAFE_BYTE_STATIC_OFFSET_BASE && scaled & 7 == 0);
        // The second copy registered under the SAME scaled offset: a shared
        // row, the shape a name-keyed mint produces.
        remember_unsafe_static_field_offset(vm, scaled, c1, 0);
        ctx.set_static_field(c0, 0, Value::Int(0x1122_3344));
        ctx.set_static_field(c1, 0, Value::Int(0x5566_7788));
        let m0 = ctx.get_class_mirror(c0);
        let m1 = ctx.get_class_mirror(c1);
        let t0 = unsafe_static_sub_target(&ctx, Some(m0), scaled + 1);
        assert_eq!(t0, Some((c0, 0, 1)));
        let t1 = unsafe_static_sub_target(&ctx, Some(m1), scaled + 1);
        assert_eq!(t1, Some((c1, 0, 1)));
        // No base: the latest registration, as before.
        let latest = unsafe_static_sub_target(&ctx, None, scaled + 1);
        assert_eq!(latest, Some((c1, 0, 1)));
        // The exact offset is never a sub-target.
        assert_eq!(unsafe_static_sub_target(&ctx, Some(m0), scaled), None);
        let g0 = unsafe_static_sub_get(&ctx, Some(m0), scaled + 1);
        assert_eq!(g0, Some(Value::Int(0x0011_2233)));
        let g1 = unsafe_static_sub_get(&ctx, Some(m1), scaled + 1);
        assert_eq!(g1, Some(Value::Int(0x0055_6677)));
        // An int store at byte 2 of copy 0 (clamped to the field's two upper
        // bytes) leaves copy 1 untouched.
        let wrote = unsafe_static_sub_put(&mut ctx, Some(m0), scaled + 2, Value::Int(0x7f));
        assert!(wrote);
        assert_eq!(ctx.get_static_field(c0, 0), Value::Int(0x007f_3344));
        assert_eq!(ctx.get_static_field(c1, 0), Value::Int(0x5566_7788));
        // A CAS on copy 1's bytes 1..3 sees copy 1's witness, not copy 0's.
        let (ex, nv) = (Value::Int(0x0055_6677), Value::Int(1));
        let swapped = unsafe_static_sub_cas(&mut ctx, Some(m1), scaled + 1, ex, nv);
        assert_eq!(swapped, Some(true));
        assert_eq!(ctx.get_static_field(c1, 0), Value::Int(0x0000_0188));
        assert_eq!(ctx.get_static_field(c0, 0), Value::Int(0x007f_3344));
        crate::forget_vm_unsafe_side_stores(vm);
    }
}
