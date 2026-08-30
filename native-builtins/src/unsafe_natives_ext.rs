// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `jdk.internal.misc.Unsafe` / `sun.misc.Unsafe` natives, the off-heap arena and the field-offset model.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

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
pub(crate) fn native_unsafe_address_size(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(std::mem::size_of::<usize>() as i32)))
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
    ctx.ensure_class_initialized(&class_name)?;
    Ok(None)
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
        native_unsafe_get_int_volatile,
    );
    r.register(
        u,
        "putIntVolatile",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile,
    );
    r.register(
        u,
        "getLongVolatile",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_volatile,
    );
    r.register(
        u,
        "putLongVolatile",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_volatile,
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
        native_unsafe_allocate_instance,
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
        native_unsafe_get_byte_mb,
    );
    r.register(
        u,
        "putBoolean",
        "(Ljava/lang/Object;JZ)V",
        native_unsafe_put_byte_mb,
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
        native_unsafe_get_int_volatile,
    );
    r.register(
        u,
        "putBooleanVolatile",
        "(Ljava/lang/Object;JZ)V",
        native_unsafe_put_int_volatile,
    );
    r.register(
        u,
        "getByteVolatile",
        "(Ljava/lang/Object;J)B",
        native_unsafe_get_int_volatile,
    );
    r.register(
        u,
        "putByteVolatile",
        "(Ljava/lang/Object;JB)V",
        native_unsafe_put_int_volatile,
    );
    r.register(
        u,
        "getShortVolatile",
        "(Ljava/lang/Object;J)S",
        native_unsafe_get_int_volatile,
    );
    r.register(
        u,
        "putShortVolatile",
        "(Ljava/lang/Object;JS)V",
        native_unsafe_put_int_volatile,
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
        native_unsafe_get_int_volatile,
    );
    r.register(
        u,
        "putCharVolatile",
        "(Ljava/lang/Object;JC)V",
        native_unsafe_put_int_volatile,
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
    r.register(u2, "<clinit>", "()V", |ctx, _args| {
        let class_name = "jdk/internal/misc/Unsafe";
        let unsafe_obj = try_alloc_concurrent_synthetic(ctx, class_name, 0)?;
        ctx.set_static_field_by_name(class_name, "theUnsafe", Value::Object(Some(unsafe_obj)));
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
    r.register_with_kind(
        u2,
        "compareAndSetByte",
        "(Ljava/lang/Object;JBB)Z",
        native_unsafe_cas_int,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "compareAndSetShort",
        "(Ljava/lang/Object;JSS)Z",
        native_unsafe_cas_int,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "compareAndExchangeByte",
        "(Ljava/lang/Object;JBB)B",
        native_unsafe_compare_and_exchange_int,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "compareAndExchangeShort",
        "(Ljava/lang/Object;JSS)S",
        native_unsafe_compare_and_exchange_int,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "getIntVolatile",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_volatile,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putIntVolatile",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "getLongVolatile",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_volatile,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register_with_kind(
        u2,
        "putLongVolatile",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_volatile,
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
        native_unsafe_allocate_instance,
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

    // allocateMemory / reallocateMemory / freeMemory — off-heap memory
    // In our managed VM, we simulate these with heap-backed byte arrays.
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

/// Lock the shard that owns `key` in a `usize`-keyed sharded map.
#[inline]
pub(crate) fn lock_unsafe_shard_usize<V>(
    map: &'static UnsafeShardedMap<usize, V>,
    key: usize,
) -> parking_lot::MutexGuard<'static, rustc_hash::FxHashMap<usize, V>> {
    map[unsafe_shard_of(&key)].lock()
}

fn unsafe_static_field_targets() -> &'static UnsafeShardedMap<usize, UnsafeStaticFieldTarget> {
    static T: std::sync::OnceLock<UnsafeShardedMap<usize, UnsafeStaticFieldTarget>> =
        std::sync::OnceLock::new();
    T.get_or_init(new_unsafe_sharded_map)
}

pub(crate) fn remember_unsafe_static_field_offset(
    offset: usize,
    class_id: ClassId,
    field_index: usize,
) {
    lock_unsafe_shard_usize(unsafe_static_field_targets(), offset).insert(
        offset,
        UnsafeStaticFieldTarget {
            class_id: class_id.as_u32(),
            field_index,
        },
    );
}

/// First-touch classification of an offset that reached the NULL-BASE fallback
/// side store, for L1 residual R5.
///
/// A null base means an ABSOLUTE ADDRESS in the JDK's contract, and HotSpot
/// SIGSEGVs on one it cannot map. Here such an access falls through to a
/// private `static_{int,long,obj}_store` keyed by the offset, which invents the
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
    let frames = ctx.capture_stack_trace(0);
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

pub(crate) fn note_unsafe_side_store_offset(
    ctx: &mut dyn NativeContext,
    offset: usize,
    site: u32,
    nargs: usize,
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    if crate::unsafe_arena_addr_is_tagged(offset as i64)
        || crate::is_synthetic_offset(offset)
        || unsafe_static_field_target(offset).is_some()
    {
        return;
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
            "UNCLASSIFIED-NULL-BASE: an Unsafe access with a null base whose              offset is neither an arena handle, nor a synthetic offset, nor a              registered static field. HotSpot reads this as an absolute address              and faults; here it lands in a private side store, so a CAS can              report success having written nowhere a reader can see. L1 R5."
        );
    }
}

fn unsafe_static_field_target(offset: usize) -> Option<(ClassId, usize)> {
    let target = lock_unsafe_shard_usize(unsafe_static_field_targets(), offset)
        .get(&offset)
        .copied()?;
    Some((ClassId::new(target.class_id), target.field_index))
}

fn unsafe_static_get(ctx: &dyn NativeContext, offset: usize) -> Option<Value> {
    let (class_id, field_index) = unsafe_static_field_target(offset)?;
    Some(ctx.get_static_field(class_id, field_index))
}

fn unsafe_static_put(ctx: &mut dyn NativeContext, offset: usize, value: Value) -> bool {
    let Some((class_id, field_index)) = unsafe_static_field_target(offset) else {
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
    let (class_id, field_index) = unsafe_static_field_target(offset)?;
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
    let (class_id, field_index) = unsafe_static_field_target(offset)?;
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
    let (class_id, field_index) = unsafe_static_field_target(offset)?;
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
    let (class_id, field_index) = unsafe_static_field_target(offset)?;
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
    let (class_id, field_index) = unsafe_static_field_target(offset)?;
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
    let (class_id, field_index) = unsafe_static_field_target(offset)?;
    let cur = ctx.get_static_field(class_id, field_index);
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
    let (class_id, field_index) = unsafe_static_field_target(offset)?;
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
    None
}

pub(crate) fn unsafe_cas_values_equal(cur: Value, expected: Value) -> bool {
    match (cur, expected) {
        (Value::Object(a), Value::Object(b)) => a == b,
        (Value::Object(None), Value::Int(0)) | (Value::Int(0), Value::Object(None)) => true,
        (Value::Object(None), Value::Long(0)) | (Value::Long(0), Value::Object(None)) => true,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        _ => false,
    }
}

// =====================================================================
// GC integration for the Unsafe / Class$Atomic side stores.
//
// `static_obj_store`, `synthetic_field_store` and `class_atomic_side_store`
// each hold live `ObjectRef`s that exist in NO heap slot — the value is kept
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

/// GC root scan hook — called from `vm/src/memory/roots.rs::collect_roots`.
/// Reports every `ObjectRef` held in an Unsafe / Class$Atomic side store so
/// the collector keeps it live (these refs live in no heap slot).
pub fn gc_scan_unsafe_side_store_roots(out: &mut Vec<cratonvm_types::ObjectRef>) {
    // Static-field fallback store (Unsafe.{CAS,get,put}Reference on a null
    // receiver — absolute-offset static Object fields). Sharded.
    for shard in static_obj_store().iter() {
        let map = shard.lock();
        for v in map.values() {
            if let Some(o) = v {
                out.push(*o);
            }
        }
    }
    // Per-object synthetic-offset store (Value-typed; only Object values are
    // heap refs — Int/Long payloads are skipped).
    {
        let map = synthetic_field_store().lock();
        for v in map.values() {
            if let Value::Object(Some(o)) = v {
                out.push(*o);
            }
        }
    }
    // Class$Atomic reflectionData / annotationType / annotationData slots.
    {
        let map = class_atomic_side_store().lock();
        for v in map.values() {
            if let Some(o) = v {
                out.push(*o);
            }
        }
    }
}

/// GC post-collection hook — called from
/// `vm/src/memory/gc.rs::update_all_roots`. Remaps every side-store `ObjectRef`
/// through the collection's pointer map (a moving GC relocates the ref; a
/// non-moving sweep's selective promotion may tenure it). Entries absent from
/// the map did not move and stay as-is.
pub fn gc_update_unsafe_side_store_refs(pointer_map: &cratonvm_types::PointerMap) {
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
        for v in map.values_mut() {
            if let Some(o) = v {
                remap(o);
            }
        }
    }
    {
        let mut map = synthetic_field_store().lock();
        for v in map.values_mut() {
            if let Value::Object(Some(o)) = v {
                remap(o);
            }
        }
    }
    {
        let mut map = class_atomic_side_store().lock();
        for v in map.values_mut() {
            if let Some(o) = v {
                remap(o);
            }
        }
    }
}

/// Throw a real `java.lang.InstantiationException`.
///
/// There is no `RuntimeError` variant for it; `Constructor.newInstance`'s
/// abstract/interface refusal in `lang_class.rs` builds the class and throws
/// the object, and this is the same shape. The TYPE is what callers catch --
/// an `IllegalStateException` in its place is missed by every
/// `catch (InstantiationException)` in the world.
fn unsafe_instantiation_exception(ctx: &mut dyn NativeContext) -> MethodCallResult {
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

/// True if `class_id` is (or descends from) `java.lang.Record`.
///
/// `objectFieldOffset` refuses a record's component field, and this is how the
/// declaring class is recognised without a dedicated `is_record` accessor: a
/// record class always has `java/lang/Record` on its superclass chain, and
/// nothing else does.
fn declaring_class_is_record(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    let mut cur = ctx.superclass_of(class_id);
    let mut hops = 0;
    while let Some(c) = cur {
        if ctx.class_name_of_id(c).as_deref() == Some("java/lang/Record") {
            return true;
        }
        hops += 1;
        if hops > 64 {
            return false;
        }
        cur = ctx.superclass_of(c);
    }
    false
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
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some("Unsafe.objectFieldOffset: null Field".to_string()),
        }
        .into());
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
        if is_static {
            return Err(RuntimeError::IllegalArgumentException {
                message: "not an instance field".to_string(),
            }
            .into());
        }

        // A RECORD's component field and a HIDDEN class's field have no offset
        // a caller may address, and `sun.misc.Unsafe` refuses both so that a
        // deserializer cannot write one.
        //
        // THE TWO SPELLINGS DO NOT SHARE THIS CONTRACT. Measured, HotSpot
        // 25.0.4+7:
        //
        //   sun.misc.Unsafe.objectFieldOffset(record component)  -> UOE
        //   jdk.internal.misc.Unsafe.objectFieldOffset(the same) -> an offset
        //
        // so the shared native has to ask which door it came through, and the
        // receiver is the only thing that says. Applying the refusal to both
        // spellings replaced one wrong answer with another.
        if unsafe_receiver_is_sun_misc(ctx, args) {
            if let Some((class_id, _)) = crate::lang_class::field_class_and_name(ctx, *field_obj) {
                if declaring_class_is_record(ctx, class_id) {
                    return Err(RuntimeError::UnsupportedOperationException {
                        message: "can't get field offset on a record class".to_string(),
                    }
                    .into());
                }
                // A hidden class is registered under `<this_class>/0x<n>` --
                // the class store's own naming convention, documented at
                // `lang_class.rs`'s `split_hidden_suffix`, and the same thing
                // `RJdkHidden.java` keys its assertion on.
                if ctx
                    .class_name_of_id(class_id)
                    .map(|n| n.contains("/0x"))
                    .unwrap_or(false)
                {
                    return Err(RuntimeError::UnsupportedOperationException {
                        message: "can't get field offset on a hidden class".to_string(),
                    }
                    .into());
                }
            }
        }

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

        return Ok(Some(Value::Long(effective_slot as i64)));
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
    let Some(field_arg) = args.get(1) else {
        return Err(RuntimeError::NullPointerException {
            message: Some("Unsafe.staticFieldOffset: null Field".to_string()),
        }
        .into());
    };
    let Value::Object(Some(field_obj)) = field_arg else {
        return Err(RuntimeError::NullPointerException {
            message: Some("Unsafe.staticFieldOffset: null Field".to_string()),
        }
        .into());
    };
    let (is_static, meta_class_id, meta_slot, _desc) =
        crate::lang_class::read_field_meta(ctx, *field_obj);
    if !is_static {
        return Err(RuntimeError::IllegalArgumentException {
            message: "not a static field".to_string(),
        }
        .into());
    }

    let (class_id, field_name) = crate::lang_class::field_class_and_name(ctx, *field_obj)
        .unwrap_or_else(|| (meta_class_id, format!("slot#{meta_slot}")));
    let field_index = ctx
        .static_field_index_by_name(class_id, &field_name)
        .unwrap_or(meta_slot);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let synthetic_key = format!("static:{field_name}");
    let offset = synthetic_offset_for(&class_name, &synthetic_key);
    remember_unsafe_static_field_offset(offset, class_id, field_index);
    Ok(Some(Value::Long(offset as i64)))
}

/// `Unsafe.arrayBaseOffset(Class)` — byte offset of element 0 from the
/// array object's base. Real HotSpot returns 16 on 64-bit (object header
/// size). JCTools `UnsafeRefArrayAccess.<clinit>` and similar consumers
/// use this value as-is for pointer arithmetic, but nothing in our VM
/// actually dereferences the computed address; we only need the static
/// initializers to accept the value. Returning 16 matches the 64-bit
/// HotSpot layout and keeps JCTools happy.
pub(crate) fn native_unsafe_array_base_offset(
    _ctx: &mut dyn NativeContext,
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
            message: Some("Unsafe.arrayBaseOffset: null class".to_string()),
        }
        .into());
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
/// Non-array classes or unknown shapes default to 1 (matches the previous
/// catch-all behaviour for defensive callers).
pub(crate) fn native_unsafe_array_index_scale(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Unsafe this, args[1] = Class mirror of the array type.
    // MEASURED, HotSpot 25.0.4+7: a null class is a `NullPointerException`,
    // not the catch-all scale of 1. See `native_unsafe_array_base_offset`.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some("Unsafe.arrayIndexScale: null class".to_string()),
        }
        .into());
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

pub(crate) fn native_unsafe_cas_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), expected(int), new(int)]
    let offset = unsafe_offset(args, 2);
    let expected = args.get(3).copied().unwrap_or(Value::Int(0));
    let new_val = args.get(4).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(ok) = unsafe_static_cas(ctx, offset, expected, new_val) {
                return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
            }
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let mut map = lock_unsafe_shard_usize(static_int_store(), offset);
            let cur = *map.entry(offset).or_insert(0);
            let ex = if let Value::Int(e) = expected { e } else { 0 };
            let nv = if let Value::Int(n) = new_val { n } else { 0 };
            let ok = cur == ex;
            if ok {
                map.insert(offset, nv);
            }
            return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
        }
    };
    if is_synthetic_offset(offset) {
        // Round-9 Bug 7: pass ctx so the side store can use a GC-stable
        // identity hash instead of a raw pointer.
        let ok = synthetic_cas(ctx, obj, offset, expected, new_val);
        return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // Route through the locked CAS path so concurrent threads can't
        // interleave inside the load+compare+store. See the matching note
        // in `native_unsafe_cas_object` above.
        // Bounds-check the decoded index in-crate before it reaches the
        // host accessor; OOB offset => CAS fails (no heap touch).
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        let result = ctx.compare_and_swap_field(obj, idx, expected, new_val);
        return Ok(Some(Value::Int(if result { 1 } else { 0 })));
    }
    let result = ctx.compare_and_swap_field(obj, offset, expected, new_val);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_unsafe_cas_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), expected(long), new(long)]
    let offset = unsafe_offset(args, 2);
    let expected = args.get(3).copied().unwrap_or(Value::Long(0));
    let new_val = args.get(4).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some((class_id, field_index)) = unsafe_static_field_target(offset) {
                let cur = ctx.get_static_field(class_id, field_index);
                let ok = unsafe_cas_values_equal(cur, expected);
                if ok {
                    ctx.set_static_field(class_id, field_index, new_val);
                }
                return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
            }
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let mut map = lock_unsafe_shard_usize(static_long_store(), offset);
            let cur = *map.entry(offset).or_insert(0);
            let ex = if let Value::Long(e) = expected { e } else { 0 };
            let nv = if let Value::Long(n) = new_val { n } else { 0 };
            let ok = cur == ex;
            if ok {
                map.insert(offset, nv);
            }
            return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
        }
    };
    if is_synthetic_offset(offset) {
        // Round-9 Bug 7: pass ctx so the side store can use a GC-stable
        // identity hash instead of a raw pointer.
        let ok = synthetic_cas(ctx, obj, offset, expected, new_val);
        return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // Route through the locked CAS path. See `native_unsafe_cas_object`.
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
    // T19_H6_CAS_DIAG: temporary probe — log the first few CAS-fails on long
    // instance fields so we can see whether the heap returned a Double-tagged
    // bit pattern (or Object(None)) for a long slot. Rate-limited to 5 entries
    // to avoid flooding when a livelock fires the CAS millions of times.
    if !result {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CAS_DIAG_COUNT: AtomicU64 = AtomicU64::new(0);
        let n = CAS_DIAG_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 5 {
            let cid = ctx.class_id_of_object(obj);
            let cls = ctx
                .class_name_of_id(cid)
                .unwrap_or_else(|| "<?>".to_string());
            let current = ctx.get_field_volatile(obj, offset);
            tracing::warn!(
                target: "cas_diag",
                "T19_H6_CAS_DIAG cas_long FAIL #{} class={} slot={} current={:?} expected={:?} new={:?}",
                n, cls, offset, current, expected, new_val
            );
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let mut map = lock_unsafe_shard_usize(static_obj_store(), offset);
            let cur = *map.entry(offset).or_insert(None);
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
                map.insert(offset, nv);
            }
            return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
        }
    };
    // Synthetic-offset CAS — routed via per-object side store so lazy-init
    // guards on JDK-internal fields (Class.reflectionData, ServicesCatalog,
    // AbstractClassLoaderValue) don't livelock when the field's real heap
    // slot is unknown to our layout.  See `synthetic_offset_for` above.
    if is_synthetic_offset(offset) {
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
                    _ => Value::Int(0),
                }));
            }
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let map = lock_unsafe_shard_usize(static_int_store(), offset);
            return Ok(Some(Value::Int(map.get(&offset).copied().unwrap_or(0))));
        }
    };
    if is_synthetic_offset(offset) {
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
        Ok(Some(ctx.get_field_volatile(obj, offset)))
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            lock_unsafe_shard_usize(static_int_store(), offset).insert(offset, v);
            return Ok(None);
        }
    };
    if is_synthetic_offset(offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
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
                    _ => Value::Long(0),
                }));
            }
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let map = lock_unsafe_shard_usize(static_long_store(), offset);
            return Ok(Some(Value::Long(map.get(&offset).copied().unwrap_or(0))));
        }
    };
    if is_synthetic_offset(offset) {
        return Ok(Some(match synthetic_get(ctx, obj, offset) {
            Value::Long(v) => Value::Long(v),
            Value::Int(v) => Value::Long(v as i64),
            _ => Value::Long(0),
        }));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => Ok(Some(ctx.get_array_element(obj, idx))),
            None => Ok(Some(Value::Long(0))),
        }
    } else {
        Ok(Some(ctx.get_field_volatile(obj, offset)))
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            lock_unsafe_shard_usize(static_long_store(), offset).insert(offset, v);
            return Ok(None);
        }
    };
    if is_synthetic_offset(offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let map = lock_unsafe_shard_usize(static_obj_store(), offset);
            return Ok(Some(Value::Object(
                map.get(&offset).copied().unwrap_or(None),
            )));
        }
    };
    if is_synthetic_offset(offset) {
        return Ok(Some(recover_object_arg(synthetic_get(ctx, obj, offset))));
    }
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => ctx.get_array_element(obj, idx),
            None => Value::Object(None),
        }
    } else {
        ctx.get_field_volatile(obj, offset)
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            lock_unsafe_shard_usize(static_obj_store(), offset).insert(offset, v);
            return Ok(None);
        }
    };
    if is_synthetic_offset(offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let map = lock_unsafe_shard_usize(static_obj_store(), offset);
            return Ok(Some(Value::Object(
                map.get(&offset).copied().unwrap_or(None),
            )));
        }
    };
    if is_synthetic_offset(offset) {
        return Ok(Some(recover_object_arg(synthetic_get(ctx, obj, offset))));
    }
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => ctx.get_array_element(obj, idx),
            None => Value::Object(None),
        }
    } else {
        ctx.get_field(obj, offset)
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            lock_unsafe_shard_usize(static_obj_store(), offset).insert(offset, v);
            return Ok(None);
        }
    };
    if is_synthetic_offset(offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
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
fn unsafe_read_bytes_from_array(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    width: usize,
) -> Option<Vec<u8>> {
    use cratonvm_types::ArrayElementType as Aet;
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let elem_type = ctx.heap_element_type_of(obj);
    let elem_size: usize = match elem_type {
        Aet::Byte | Aet::Boolean => 1,
        Aet::Char | Aet::Short => 2,
        Aet::Int | Aet::Float => 4,
        Aet::Long | Aet::Double => 8,
        // Reference arrays have no byte-addressable element storage.
        Aet::Reference => return None,
    };
    // Array byte offsets always start at ABASE (16); see
    // `unsafe_array_index_from_offset` / `native_unsafe_array_base_offset`.
    const ABASE: usize = 16;
    if offset < ABASE {
        return None;
    }
    let rel = offset - ABASE; // bytes from element 0
    let len = ctx.array_length(obj); // element COUNT
    let total_bytes = len.checked_mul(elem_size)?;
    if rel.checked_add(width)? > total_bytes {
        return None;
    }
    let mut bytes = Vec::with_capacity(width);
    for k in 0..width {
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
            _ => return None,
        };
        bytes.push(((elem_bits >> (8 * byte_in_elem)) & 0xFF) as u8);
    }
    Some(bytes)
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
    ($name:ident, $width:expr, $assemble:expr) => {
        pub(crate) fn $name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
                if let Some(bytes) = unsafe_read_bytes_from_array(ctx, obj, offset, $width) {
                    let big_endian = unsafe_big_endian_arg(args);
                    let v: i64 = $assemble(&bytes, big_endian);
                    return Ok(Some(Value::Int(v as i32)));
                }
            }
            // Not a primitive-array/direct-memory target: generic element/field access.
            native_unsafe_get_int(ctx, args)
        }
    };
}

// `getShort` → signed 16-bit; `getChar` → unsigned 16-bit; `getInt` → 32-bit.
unsafe_multibyte_get!(native_unsafe_get_short_mb, 2, asm_i16);

unsafe_multibyte_get!(native_unsafe_get_char_mb, 2, asm_u16);

unsafe_multibyte_get!(native_unsafe_get_int_mb, 4, asm_i32);

/// 64-bit `Unsafe` get for `byte[]`-backed memory (`getLong`/`getLongUnaligned`).
pub(crate) fn native_unsafe_get_long_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
        if let Some(b) = unsafe_read_bytes_from_array(ctx, obj, offset, 8) {
            let big_endian = unsafe_big_endian_arg(args);
            let v = if big_endian {
                i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
            } else {
                i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
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
    for (i, &b) in bytes.iter().enumerate() {
        // Byte-array elements round-trip as sign-extended Int.
        ctx.set_array_element(obj, start + i, Value::Int(b as i8 as i32));
    }
    true
}

macro_rules! unsafe_multibyte_put {
    ($name:ident, $width:expr, $generic:ident) => {
        pub(crate) fn $name(
            ctx: &mut dyn NativeContext,
            args: &[Value],
        ) -> MethodCallResult {
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
                }
            }
            $generic(ctx, args)
        }
    };
}

unsafe_multibyte_put!(native_unsafe_put_short_mb, 2, native_unsafe_put_int);

unsafe_multibyte_put!(native_unsafe_put_char_mb, 2, native_unsafe_put_int);

unsafe_multibyte_put!(native_unsafe_put_int_mb, 4, native_unsafe_put_int);

/// 64-bit `Unsafe` put for `byte[]`-backed memory.
pub(crate) fn native_unsafe_put_long_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let map = lock_unsafe_shard_usize(static_int_store(), offset);
            return Ok(Some(Value::Int(map.get(&offset).copied().unwrap_or(0))));
        }
    };
    if is_synthetic_offset(offset) {
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
        Ok(Some(ctx.get_field(obj, offset)))
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            lock_unsafe_shard_usize(static_int_store(), offset).insert(offset, v);
            return Ok(None);
        }
    };
    if is_synthetic_offset(offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
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
        Some(_) => native_unsafe_get_int(ctx, args),
    }
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
        Some(_) => native_unsafe_put_int(ctx, args),
    }
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let map = lock_unsafe_shard_usize(static_long_store(), offset);
            return Ok(Some(Value::Long(map.get(&offset).copied().unwrap_or(0))));
        }
    };
    if is_synthetic_offset(offset) {
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
        ctx.get_field(obj, offset)
    };
    // Coerce non-Long values to Long(0) so stack type stays valid; PD0$4's
    // `lstore` would otherwise see an Object and mis-interpret the slot.
    match v {
        Value::Long(_) => Ok(Some(v)),
        Value::Int(i) => Ok(Some(Value::Long(i as i64))),
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            lock_unsafe_shard_usize(static_long_store(), offset).insert(offset, v);
            return Ok(None);
        }
    };
    if is_synthetic_offset(offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
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
    if let Some(Value::Object(Some(class_obj))) = args.get(1) {
        if let Some(name) = crate::lang_class::mirror_class_name(ctx, *class_obj) {
            let is_primitive = matches!(
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
            );
            if is_primitive || name.starts_with('[') {
                return unsafe_instantiation_exception(ctx);
            }
        }
        if let Some(cid) =
            ctx.class_id_from_mirror(*class_obj)
                .or_else(|| match ctx.get_field(*class_obj, 0) {
                    Value::Int(c) if c >= 0 => Some(cratonvm_types::ClassId::new(c as u32)),
                    _ => None,
                })
        {
            let ACC_INTERFACE = cratonvm_types::access_flags::ACC_INTERFACE;
            let ACC_ABSTRACT = cratonvm_types::access_flags::ACC_ABSTRACT;
            let flags = ctx.class_access_flags(cid);
            if flags & (ACC_INTERFACE | ACC_ABSTRACT) != 0 {
                return unsafe_instantiation_exception(ctx);
            }
        }
    }
    // Read class name from mirror, allocate without calling <init>
    if let Some(Value::Object(Some(class_obj))) = args.get(1) {
        let cid_opt =
            ctx.class_id_from_mirror(*class_obj)
                .or_else(|| match ctx.get_field(*class_obj, 0) {
                    Value::Int(cid) if cid >= 0 => Some(cratonvm_types::ClassId::new(cid as u32)),
                    _ => None,
                });
        if let Some(class_id) = cid_opt {
            if let Some(name) = ctx.class_name_of_id(class_id) {
                if let Some(obj) = ctx.allocate_instance(&name) {
                    return Ok(Some(Value::Object(Some(obj))));
                }
            }
        }
    }
    Ok(Some(Value::Object(None)))
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
    // JDK spec: if interrupted, park returns immediately (no exception, flag NOT cleared)
    if ctx.is_interrupted(false) {
        return Ok(None);
    }
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

    let timeout = if time == 0 {
        None // park indefinitely
    } else if is_absolute {
        // time is absolute deadline in milliseconds since epoch
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let remaining = (time - now_ms).max(0) as u64;
        Some(std::time::Duration::from_millis(remaining))
    } else if time < 0 {
        // Relative `Unsafe.park` time is nanoseconds. Timed JDK wait loops can
        // race past their deadline and pass a negative remainder; HotSpot
        // returns promptly. Casting that negative value to u64 would park for
        // centuries and turn a bounded Future.get(timeout) into a hang.
        Some(std::time::Duration::ZERO)
    } else {
        // time is relative in nanoseconds
        Some(std::time::Duration::from_nanos(time as u64))
    };

    if let Some(yielded) = try_yield_virtual_park(ctx, timeout) {
        return yielded;
    }
    ctx.park(timeout);
    Ok(None)
}

fn native_unsafe_unpark(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe, thread_obj]
    if let Some(Value::Object(Some(thread_obj))) = args.get(1) {
        ctx.unpark(*thread_obj);
    } else {
    }
    Ok(None)
}

pub(crate) fn native_unsafe_get_and_add_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), delta(int)]
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let mut map = lock_unsafe_shard_usize(static_int_store(), offset);
            let slot = map.entry(offset).or_insert(0);
            let old = *slot;
            *slot = old.wrapping_add(delta);
            return Ok(Some(Value::Int(old)));
        }
    };
    if is_synthetic_offset(offset) {
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
        let current = ctx.get_array_element(obj, idx);
        if let Value::Int(old) = current {
            ctx.set_array_element(obj, idx, Value::Int(old.wrapping_add(delta)));
            return Ok(Some(Value::Int(old)));
        }
        return Ok(Some(Value::Int(0)));
    }
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
    let offset = unsafe_offset(args, 2);
    let new_val = args.get(3).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let nv = if let Value::Int(n) = new_val { n } else { 0 };
            if let Some(prev) = unsafe_static_get_and_set_int(ctx, offset, nv) {
                return Ok(Some(Value::Int(prev)));
            }
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let mut map = lock_unsafe_shard_usize(static_int_store(), offset);
            let prev = map.insert(offset, nv).unwrap_or(0);
            return Ok(Some(Value::Int(prev)));
        }
    };
    if is_synthetic_offset(offset) {
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
        let prev = ctx.get_array_element(obj, idx);
        ctx.set_array_element(obj, idx, new_val);
        return Ok(Some(prev));
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
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let mut b = [0u8; 4];
        if ctx.copy_from_native_memory(addr, &mut b) {
            return Ok(Some(Value::Float(f32::from_le_bytes(b))));
        }
    }
    native_unsafe_get_float(ctx, args)
}

/// Write half of [`native_unsafe_get_float_mb`].
pub(crate) fn native_unsafe_put_float_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    native_unsafe_put_float(ctx, args)
}

/// `Unsafe.getDouble(Object, long)` with the null-base arm routed off-heap.
/// See [`native_unsafe_get_float_mb`].
pub(crate) fn native_unsafe_get_double_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let mut b = [0u8; 8];
        if ctx.copy_from_native_memory(addr, &mut b) {
            return Ok(Some(Value::Double(f64::from_le_bytes(b))));
        }
    }
    native_unsafe_get_double(ctx, args)
}

/// Write half of [`native_unsafe_get_double_mb`].
pub(crate) fn native_unsafe_put_double_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    if is_synthetic_offset(offset) {
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
        ctx.get_field(obj, offset)
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
    if is_synthetic_offset(offset) {
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
    if is_synthetic_offset(offset) {
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
        ctx.get_field(obj, offset)
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
    if is_synthetic_offset(offset) {
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
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

fn native_unsafe_get_and_add_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let mut map = lock_unsafe_shard_usize(static_long_store(), offset);
            let slot = map.entry(offset).or_insert(0);
            let old = *slot;
            *slot = old.wrapping_add(delta);
            return Ok(Some(Value::Long(old)));
        }
    };
    if is_synthetic_offset(offset) {
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
        let current = ctx.get_array_element(obj, idx);
        if let Value::Long(old) = current {
            ctx.set_array_element(obj, idx, Value::Long(old.wrapping_add(delta)));
            return Ok(Some(Value::Long(old)));
        }
        return Ok(Some(Value::Long(0)));
    }
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
    let offset = unsafe_offset(args, 2);
    let new_val = args.get(3).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let nv = if let Value::Long(n) = new_val { n } else { 0 };
            if let Some(prev) = unsafe_static_get_and_set_long(ctx, offset, nv) {
                return Ok(Some(Value::Long(prev)));
            }
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let mut map = lock_unsafe_shard_usize(static_long_store(), offset);
            let prev = map.insert(offset, nv).unwrap_or(0);
            return Ok(Some(Value::Long(prev)));
        }
    };
    if is_synthetic_offset(offset) {
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
        let prev = ctx.get_array_element(obj, idx);
        ctx.set_array_element(obj, idx, new_val);
        return Ok(Some(prev));
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
            note_unsafe_side_store_offset(ctx, offset, line!(), args.len());
            let mut map = lock_unsafe_shard_usize(static_obj_store(), offset);
            let prev = map.insert(offset, nv).unwrap_or(None);
            return Ok(Some(Value::Object(prev)));
        }
    };
    if is_synthetic_offset(offset) {
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
        let prev = ctx.get_array_element(obj, idx);
        ctx.set_array_element(obj, idx, new_val);
        return Ok(Some(prev));
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
            _ => ((cur.as_int().unwrap_or(0) as i64) as u64).to_le_bytes(),
        };
        while byte_in_elem < width && consumed < bytes.len() {
            elem_bytes[byte_in_elem] = bytes[consumed];
            byte_in_elem += 1;
            consumed += 1;
        }
        let raw = u64::from_le_bytes(elem_bytes);
        let new_val =
            match et {
                cratonvm_types::ArrayElementType::Boolean
                | cratonvm_types::ArrayElementType::Byte => Value::Int(raw as u8 as i8 as i32),
                cratonvm_types::ArrayElementType::Char => Value::Int(raw as u16 as i32),
                cratonvm_types::ArrayElementType::Short => Value::Int(raw as u16 as i16 as i32),
                cratonvm_types::ArrayElementType::Int => Value::Int(raw as u32 as i32),
                cratonvm_types::ArrayElementType::Float => Value::Float(f32::from_bits(raw as u32)),
                cratonvm_types::ArrayElementType::Long
                | cratonvm_types::ArrayElementType::Double => Value::Long(raw as i64),
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
        // Object-field (non-array) target: bounded slot fill. `bytes` is capped
        // above by MAX_UNSAFE_COPY_SIZE.
        let fill_value = Value::Int(value as i32);
        for i in 0..bytes {
            ctx.set_field(obj_ref, offset + i, fill_value);
        }
    }
    Ok(None)
}

/// Unsafe.allocateMemory(long) — simulate off-heap memory as a heap byte array.
/// Returns a synthetic "address" (actually an object reference pointer cast to long).
pub(crate) fn native_unsafe_allocate_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let size = match args.get(1) {
        Some(Value::Long(s)) => *s as usize,
        Some(Value::Int(s)) => *s as usize,
        _ => 0,
    };
    // Allocate a byte array on the managed heap to simulate off-heap memory
    let arr = ctx.new_ref_array(ClassId::new(0), size.max(1));
    Ok(Some(Value::Long(arr.as_ptr() as i64)))
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

/// Unsafe.reallocateMemory(long, long) — simulate reallocation.
pub(crate) fn native_unsafe_allocate_memory_realloc(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Just allocate a new block (old one will be GC'd)
    let new_size = match args.get(2) {
        Some(Value::Long(s)) => *s as usize,
        Some(Value::Int(s)) => *s as usize,
        _ => 0,
    };
    let arr = ctx.new_ref_array(ClassId::new(0), new_size.max(1));
    Ok(Some(Value::Long(arr.as_ptr() as i64)))
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
    use std::collections::{BTreeMap, HashSet};

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
    /// First handle handed out: the tag OR-ed onto the historical 2^36 base, so
    /// the low bits (and existing reasoning/logs about them) are unchanged.
    const ARENA_BASE: i64 = ARENA_TAG | 0x10_0000_0000;

    struct Arena {
        bytes: Vec<u8>,
    }

    pub(super) struct ArenaStore {
        inner: RwLock<BTreeMap<i64, Arena>>,
        next_addr: Mutex<i64>,
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

    impl ArenaStore {
        fn new() -> Self {
            Self {
                inner: RwLock::new(BTreeMap::new()),
                next_addr: Mutex::new(ARENA_BASE),
                translated: cratonvm_types::lock_order::OrderedPlMutex::new(
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
            let addr = {
                let mut c = self.next_addr.lock();
                let a = *c;
                *c = c.saturating_add(((size.max(1) + 15) & !15) as i64);
                a
            };
            self.inner.write().insert(addr, Arena { bytes });
            Some(addr)
        }

        pub(super) fn allocate(&self, size: usize) -> i64 {
            self.try_allocate(size).unwrap_or(0)
        }

        #[cfg(test)]
        pub(super) fn block_is_translated(&self, addr: i64) -> bool {
            self.translated.lock().contains_key(&addr)
        }

        /// `None` when the resize could not be allocated -- see
        /// [`Self::try_allocate`]. The block is left UNTOUCHED on failure,
        /// which is what `realloc(3)` guarantees and what the JDK's
        /// `reallocateMemory` contract relies on when it throws.
        pub(super) fn try_reallocate(&self, addr: i64, new_size: usize) -> Option<i64> {
            let mut inner = self.inner.write();
            let cur_len = inner.get(&addr).map(|a| a.bytes.len()).unwrap_or(0);
            if new_size > cur_len {
                match inner.get_mut(&addr) {
                    Some(a) => a.bytes.try_reserve_exact(new_size - cur_len).ok()?,
                    None => {
                        // No existing block: allocating a fresh one, so probe
                        // the request before `reallocate` commits to it.
                        let mut probe: Vec<u8> = Vec::new();
                        probe.try_reserve_exact(new_size).ok()?;
                    }
                }
            }
            drop(inner);
            Some(self.reallocate(addr, new_size))
        }

        pub(super) fn reallocate(&self, addr: i64, new_size: usize) -> i64 {
            // BEFORE the resize, which is what may move the buffer.
            if let Some(n) = self.translated.lock().get(&addr).copied() {
                STALE_ON_REALLOC.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    target: "cratonvm::unsafe_arena",
                    handle = format!("{addr:#x}"),
                    translations = n,
                    new_size,
                    "Unsafe-arena block is being RESIZED while native code holds a                      real pointer into it -- `Vec::resize` may move the buffer,                      after which that pointer names memory the allocator has                      reclaimed"
                );
            }
            let mut inner = self.inner.write();
            let old = inner.remove(&addr);
            let mut bytes = old.map(|a| a.bytes).unwrap_or_default();
            bytes.resize(new_size, 0u8);
            inner.insert(addr, Arena { bytes });
            addr
        }

        pub(super) fn free(&self, addr: i64) {
            if let Some(n) = self.translated.lock().remove(&addr) {
                STALE_ON_FREE.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    target: "cratonvm::unsafe_arena",
                    handle = format!("{addr:#x}"),
                    translations = n,
                    "Unsafe-arena block is being FREED while native code holds a                      real pointer into it -- the `Vec` is dropped and its memory                      returned to the process allocator"
                );
            }
            self.inner.write().remove(&addr);
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
            let (base, offset) = match Self::locate(&inner, addr) {
                Some(v) => v,
                None => return false,
            };
            let arena = match inner.get(&base) {
                Some(a) => a,
                None => return false,
            };
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
            let (base, offset) = match Self::locate(&inner, addr) {
                Some(v) => v,
                None => return false,
            };
            let arena = match inner.get_mut(&base) {
                Some(a) => a,
                None => return false,
            };
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

    pub(super) struct InvokedCleaners {
        seen: Mutex<HashSet<usize>>,
    }

    impl InvokedCleaners {
        fn new() -> Self {
            Self {
                seen: Mutex::new(HashSet::new()),
            }
        }

        pub(super) fn mark(&self, addr: usize) -> bool {
            self.seen.lock().insert(addr)
        }
    }

    pub(super) fn cleaners() -> &'static InvokedCleaners {
        use std::sync::OnceLock;
        static S: OnceLock<InvokedCleaners> = OnceLock::new();
        S.get_or_init(InvokedCleaners::new)
    }
}

pub(crate) fn unsafe_arena_allocate(size: usize) -> i64 {
    unsafe_arena::store().allocate(size)
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

/// `(translations, stale_on_realloc, stale_on_free)` — the lifetime audit for
/// real pointers handed out of the Unsafe arena.
///
/// # Why this exists
///
/// `unsafe_arena_real_ptr` is the one place the arena's backing store is exposed
/// rather than copied, and it has two production callers, both in `jni.rs`:
/// `jni_long_arg_bits` (a tagged handle arriving as a JNI `jlong` argument —
/// netty-tcnative's `SSL.bioWrite(long bio, long address, int len)` is the named
/// case) and `direct_buffer_native_address`. **Both discard the `remaining`
/// bound**, so the callee receives a bare pointer into a `Vec<u8>` with no
/// length, and nothing checks what it writes.
///
/// Two ways that becomes a write into unrelated memory:
///
/// * the callee writes past the block — nothing bounds it;
/// * the block is `reallocate`d or `free`d while the pointer is outstanding.
///   `reallocate` is `Vec::resize`, which may MOVE the buffer; `free` drops it.
///   Either way the pointer then names memory the process allocator has taken
///   back.
///
/// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` is looking for a
/// writer that puts an **8-byte arena-pointer-shaped value onto a live object's
/// header**, and has eliminated every managed store path, the allocator, the
/// slide and `copy_to_native_memory`. This path was not in that table: its last
/// row is a write *through* a tagged handle, which `ArenaStore::copy_in` bounds
/// into the block's own `Vec` and which therefore cannot reach the Java heap at
/// all. The hazard is the mirror image — the handle being TRANSLATED and the
/// bound dropped.
///
/// **Nonzero `stale_on_*` means the mechanism is live on this workload.** Zero
/// does not clear the path, because the unbounded-write half leaves no trace
/// here.
#[cfg(test)]
pub(crate) fn unsafe_arena_block_is_translated(addr: i64) -> bool {
    unsafe_arena::store().block_is_translated(addr)
}

pub fn unsafe_arena_translation_stats() -> (u64, u64, u64) {
    (
        unsafe_arena::TRANSLATIONS.load(std::sync::atomic::Ordering::Relaxed),
        unsafe_arena::STALE_ON_REALLOC.load(std::sync::atomic::Ordering::Relaxed),
        unsafe_arena::STALE_ON_FREE.load(std::sync::atomic::Ordering::Relaxed),
    )
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

pub(crate) fn unsafe_arena_free(addr: i64) {
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

pub(crate) fn unsafe_cleaner_mark(addr: usize) -> bool {
    unsafe_arena::cleaners().mark(addr)
}

/// Shim that unsafe_natives uses to forward byte / short getAndAdd
/// requests into the existing int-width implementation.
pub(crate) fn native_unsafe_get_and_add_int_shim(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_unsafe_get_and_add_int(ctx, args)
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
                return Ok(Some(Value::Long(slot as i64)));
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
                    return Ok(Some(Value::Long(f.slot_index as i64)));
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
    let synthetic = synthetic_offset_for(&cname, &field_name);
    tracing::warn!(
        target: "cratonvm::unsafe",
        "objectFieldOffset1: field {field_name:?} not found on class {cname:?} — minting synthetic offset {synthetic:#x} (CAS routed via side store)"
    );
    Ok(Some(Value::Long(synthetic as i64)))
}

pub(crate) fn unsafe_compare_exchange_static_field(
    ctx: &mut dyn NativeContext,
    offset: usize,
    expected: Value,
    update: Value,
) -> Option<Value> {
    let (class_id, field_index) = unsafe_static_field_target(offset)?;
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
            ctx.get_field_volatile(obj, index)
        };
        if !unsafe_cas_values_equal(old.clone(), expected.clone()) {
            return old;
        }
        if ctx.compare_and_swap_field(obj, index, expected.clone(), update.clone()) {
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
                _ => Value::Int(expected),
            }));
        }
    }
    if let Some(obj_ref) = obj {
        let old = if is_synthetic_offset(offset) {
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
                offset
            };
            unsafe_compare_exchange_heap_slot(ctx, obj_ref, index, expected_value, update_value)
        };
        return Ok(Some(match old {
            Value::Int(v) => Value::Int(v),
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
                _ => Value::Long(expected),
            }));
        }
    }
    if let Some(obj_ref) = obj {
        let old = if is_synthetic_offset(offset) {
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
                offset
            };
            unsafe_compare_exchange_heap_slot(ctx, obj_ref, index, expected_value, update_value)
        };
        return Ok(Some(match old {
            Value::Long(v) => Value::Long(v),
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
        let old = if is_synthetic_offset(offset) {
            unsafe_compare_exchange_synthetic_field(ctx, obj_ref, offset, expected, update)
        } else {
            let index = if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                match unsafe_checked_array_index(ctx, obj_ref, offset) {
                    Some(i) => i,
                    None => return Ok(Some(Value::Object(None))),
                }
            } else {
                offset
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
        // A block nobody has translated must NOT be recorded -- a detector that
        // fires on every free names nothing.
        let quiet = unsafe_arena_allocate(64);
        assert!(
            !unsafe_arena_block_is_translated(quiet),
            "an untranslated block must not be recorded"
        );
        unsafe_arena_reallocate(quiet, 128);
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
            "handing a real pointer to native code must be recorded -- otherwise              `reallocate` cannot tell that it is about to move a buffer somebody              is holding"
        );

        // Resizing it is the hazard: `Vec::resize` may move the buffer.
        let (_t, r_before, _f) = unsafe_arena_translation_stats();
        unsafe_arena_reallocate(held, 4096);
        let (_t2, r_after, _f2) = unsafe_arena_translation_stats();
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
            "the record must be dropped with the block, or a later handle at the              same address inherits a warning that is not about it"
        );

        unsafe_arena_free(held);
    }

    #[test]
    fn a_live_handle_translates_to_a_pointer_that_aliases_the_arena() {
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
        let offset = thread_next_tid_offset();
        // `&mut ctx`: this test OWNS its MockNativeContext, unlike the 21
        // production call sites which already hold a `&mut dyn NativeContext`.
        note_unsafe_side_store_offset(&mut ctx, offset, line!(), 3);
        lock_unsafe_shard_usize(static_long_store(), offset).insert(offset, 1);

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
