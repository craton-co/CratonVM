#[allow(unused_imports)]
use super::*;


#[inline]
pub fn unsafe_offset_is_heap_slot(
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
pub fn unsafe_array_before_synthetic_enabled() -> bool {
    true
}

/// Does `offset` name a SYNTHETIC field slot for the non-null base `obj`, so
/// that the side store (`synthetic_get` / `synthetic_put` / `synthetic_cas`)
/// answers it?
///
/// nio21 wave 7 L3 (`docs/feature-designs/native-dispatch-architecture.md`).
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
pub fn unsafe_synthetic_field_offset(
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
pub fn native_unsafe_page_size(
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
pub const UNSAFE_ADDRESS_SIZE: i32 = std::mem::size_of::<usize>() as i32;

pub fn native_unsafe_address_size(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(UNSAFE_ADDRESS_SIZE)))
}

pub fn native_unsafe_ensure_class_initialized(
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
    // Under `--real-jdk`, initialize THE class the mirror is, by its id. The
    // name named the application class path's copy (or nothing) for a class a
    // user loader defined, so `Lookup.ensureInitialized`, the
    // `DirectMethodHandle` init barrier, `MethodHandleAccessorFactory` and
    // `LazyInitializingVarHandle` -- every JDK route through
    // `Unsafe.ensureClassInitialized` -- ran another class's `<clinit>` and
    // left this one uninitialized, while `shouldBeInitialized0` (by id) kept
    // reporting it uninitialized (interpreter round i1 wave 29, lane L4;
    // `L4W29EnsureInitializedByMirror.java`).
    // `--compatible` takes the same id route below (wave 138, lane L5), after
    // its served-frames arm; the by-name route is the `=0` kill switch's.
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
        // Interpreter round i7 wave 138, lane L5 (page
        // `i137-orch-compatible-first-call-of-a-custom-loaders-class-throws-ncdfe`):
        // the class the mirror IS, by its id, in `--compatible` too. The
        // by-name route below re-resolved the mirror's NAME through the
        // native caller's loader; for a name two user loaders define
        // (`L2W37InterfaceSelectionHot` mints a fresh loader per row, each
        // defining `IpCaller`) that lookup refuses to guess, and the
        // already-initialized class's `DirectMethodHandle` barrier answered
        // `NoClassDefFoundError: IpCaller`. HotSpot initializes the mirror's
        // own class (a no-op once initialized). Kill switch
        // `CRATONVM_UNSAFE_ENSURE_INIT_BY_MIRROR_ID=0` (parity, default ON).
        {
            ctx.ensure_class_initialized_with_class_id(class_id)?;
            return Ok(None);
        }
    }
    ctx.ensure_class_initialized(&class_name)?;
    Ok(None)
}



/// Is `receiver` a `jdk.internal.misc.Unsafe` (not `sun.misc.Unsafe`, whose
/// own frame HotSpot would list above)?
pub fn receiver_is_internal_unsafe(ctx: &dyn NativeContext, receiver: Option<&Value>) -> bool {
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
/// `L3W58ReflectiveClassInit` in `--real-jdk`, where the JDK body runs).
/// Interpreter round i2 wave 60, lane L3.
#[cold]
#[inline(never)]
pub fn ensure_class_initialized_served(
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
pub fn native_unsafe_should_be_initialized(
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
pub fn native_cds_is_sharing_enabled0(
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
pub fn native_cds_is_dumping_archive0(
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

pub fn register_unsafe_natives(r: &mut NativeMethodRegistry) {
    // census-tag: sun.misc.Unsafe — VM-internal memory/CAS primitives → Bridge.
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
    }, cratonvm_native_api::NativeKind::Bridge);
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
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        u,
        "objectFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
        native_unsafe_object_field_offset, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "staticFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
        native_unsafe_static_field_offset, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "arrayBaseOffset",
        "(Ljava/lang/Class;)I",
        native_unsafe_array_base_offset, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "arrayIndexScale",
        "(Ljava/lang/Class;)I",
        native_unsafe_array_index_scale, cratonvm_native_api::NativeKind::Bridge,
    );
    // CAS
    r.register(
        u,
        "compareAndSwapInt",
        "(Ljava/lang/Object;JII)Z",
        native_unsafe_cas_int, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "compareAndSwapLong",
        "(Ljava/lang/Object;JJJ)Z",
        native_unsafe_cas_long, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "compareAndSwapObject",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        native_unsafe_cas_object, cratonvm_native_api::NativeKind::Bridge,
    );
    // Volatile get/put
    r.register(
        u,
        "getIntVolatile",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_volatile_arena, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putIntVolatile",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile_arena, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getLongVolatile",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_volatile_arena, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putLongVolatile",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_volatile_arena, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getObjectVolatile",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putObjectVolatile",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    // Non-volatile get/put
    r.register(
        u,
        "getObject",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putObject",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getInt",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putInt",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getLong",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putLong",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_mb, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Fences
    r.register(u, "storeFence", "()V", native_unsafe_fence, cratonvm_native_api::NativeKind::Bridge);
    r.register(u, "loadFence", "()V", native_unsafe_fence, cratonvm_native_api::NativeKind::Bridge);
    r.register(u, "fullFence", "()V", native_unsafe_fence, cratonvm_native_api::NativeKind::Bridge);
    // Park/Unpark
    r.register(u, "park", "(ZJ)V", native_unsafe_park, cratonvm_native_api::NativeKind::Bridge);
    r.register(u, "unpark", "(Ljava/lang/Object;)V", native_unsafe_unpark, cratonvm_native_api::NativeKind::Bridge);
    // Compound ops
    r.register(
        u,
        "getAndAddInt",
        "(Ljava/lang/Object;JI)I",
        native_unsafe_get_and_add_int, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getAndSetInt",
        "(Ljava/lang/Object;JI)I",
        native_unsafe_get_and_set_int, cratonvm_native_api::NativeKind::Bridge,
    );
    // getAndAddLong / getAndSetLong — needed by AtomicLong
    r.register(
        u,
        "getAndAddLong",
        "(Ljava/lang/Object;JJ)J",
        native_unsafe_get_and_add_long, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getAndSetLong",
        "(Ljava/lang/Object;JJ)J",
        native_unsafe_get_and_set_long, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getAndSetObject",
        "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
        native_unsafe_get_and_set_object, cratonvm_native_api::NativeKind::Bridge,
    );
    // Primitive field access (boolean, byte, short, float, double, char)
    r.register(
        u,
        "getBoolean",
        "(Ljava/lang/Object;J)Z",
        native_unsafe_get_boolean_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putBoolean",
        "(Ljava/lang/Object;JZ)V",
        native_unsafe_put_boolean_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getByte",
        "(Ljava/lang/Object;J)B",
        native_unsafe_get_byte_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putByte",
        "(Ljava/lang/Object;JB)V",
        native_unsafe_put_byte_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getShort",
        "(Ljava/lang/Object;J)S",
        native_unsafe_get_short_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putShort",
        "(Ljava/lang/Object;JS)V",
        native_unsafe_put_short_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getFloat",
        "(Ljava/lang/Object;J)F",
        native_unsafe_get_float_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putFloat",
        "(Ljava/lang/Object;JF)V",
        native_unsafe_put_float_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getDouble",
        "(Ljava/lang/Object;J)D",
        native_unsafe_get_double_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putDouble",
        "(Ljava/lang/Object;JD)V",
        native_unsafe_put_double_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getChar",
        "(Ljava/lang/Object;J)C",
        native_unsafe_get_char_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putChar",
        "(Ljava/lang/Object;JC)V",
        native_unsafe_put_char_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    // Volatile variants for primitive types
    r.register(
        u,
        "getBooleanVolatile",
        "(Ljava/lang/Object;J)Z",
        native_unsafe_get_boolean_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putBooleanVolatile",
        "(Ljava/lang/Object;JZ)V",
        native_unsafe_put_boolean_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getByteVolatile",
        "(Ljava/lang/Object;J)B",
        native_unsafe_get_byte_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putByteVolatile",
        "(Ljava/lang/Object;JB)V",
        native_unsafe_put_byte_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getShortVolatile",
        "(Ljava/lang/Object;J)S",
        native_unsafe_get_short_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putShortVolatile",
        "(Ljava/lang/Object;JS)V",
        native_unsafe_put_short_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getFloatVolatile",
        "(Ljava/lang/Object;J)F",
        native_unsafe_get_int_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putFloatVolatile",
        "(Ljava/lang/Object;JF)V",
        native_unsafe_put_int_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getDoubleVolatile",
        "(Ljava/lang/Object;J)D",
        native_unsafe_get_long_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putDoubleVolatile",
        "(Ljava/lang/Object;JD)V",
        native_unsafe_put_long_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "getCharVolatile",
        "(Ljava/lang/Object;J)C",
        native_unsafe_get_char_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "putCharVolatile",
        "(Ljava/lang/Object;JC)V",
        native_unsafe_put_char_volatile, cratonvm_native_api::NativeKind::Bridge,
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
        unsafe_natives::native_unsafe_copy_memory_consolidated, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u,
        "setMemory",
        "(Ljava/lang/Object;JJB)V",
        native_unsafe_set_memory, cratonvm_native_api::NativeKind::Bridge,
    );
    // pageSize / addressSize — queried from the host rather than hard-coded.
    r.register(u, "pageSize", "()I", native_unsafe_page_size, cratonvm_native_api::NativeKind::Bridge);
    r.register(u, "addressSize", "()I", native_unsafe_address_size, cratonvm_native_api::NativeKind::Bridge);

    // Also register under jdk/internal/misc/Unsafe (the modern API, same implementations)
    let u2 = "jdk/internal/misc/Unsafe";
    // Same as the `sun/misc/Unsafe` entry above: JNI binding is done at
    // registry-build time, so the method genuinely has no work to do.
    r.register(
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
    }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "objectFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
        native_unsafe_object_field_offset, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "staticFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
        native_unsafe_static_field_offset, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "arrayBaseOffset",
        "(Ljava/lang/Class;)I",
        native_unsafe_array_base_offset, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "arrayIndexScale",
        "(Ljava/lang/Class;)I",
        native_unsafe_array_index_scale, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "compareAndSetInt",
        "(Ljava/lang/Object;JII)Z",
        native_unsafe_cas_int,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "compareAndSetLong",
        "(Ljava/lang/Object;JJJ)Z",
        native_unsafe_cas_long,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
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
    r.register(
        u2,
        "compareAndSetByte",
        "(Ljava/lang/Object;JBB)Z",
        native_unsafe_cas_byte,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "compareAndSetShort",
        "(Ljava/lang/Object;JSS)Z",
        native_unsafe_cas_short,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        u2,
        "compareAndExchangeByte",
        "(Ljava/lang/Object;JBB)B",
        native_unsafe_cae_byte,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        u2,
        "compareAndExchangeShort",
        "(Ljava/lang/Object;JSS)S",
        native_unsafe_cae_short,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        u2,
        "getIntVolatile",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_volatile_arena,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putIntVolatile",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile_arena,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getLongVolatile",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_volatile_arena,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putLongVolatile",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_volatile_arena,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getReferenceVolatile",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object_volatile,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
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
        native_unsafe_get_object_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getIntAcquire",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putIntRelease",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getLongAcquire",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putLongRelease",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    // Opaque variants — also same semantics for us
    r.register(
        u2,
        "getReferenceOpaque",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putReferenceOpaque",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getIntOpaque",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putIntOpaque",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_volatile, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getReference",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
        native_unsafe_get_object,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putReference",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
        native_unsafe_put_object,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getInt",
        "(Ljava/lang/Object;J)I",
        native_unsafe_get_int_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putInt",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getLong",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putLong",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getShort",
        "(Ljava/lang/Object;J)S",
        native_unsafe_get_short_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putShort",
        "(Ljava/lang/Object;JS)V",
        native_unsafe_put_short_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getChar",
        "(Ljava/lang/Object;J)C",
        native_unsafe_get_char_mb,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
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
        native_unsafe_get_int_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getIntUnaligned",
        "(Ljava/lang/Object;JZ)I",
        native_unsafe_get_int_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putIntUnaligned",
        "(Ljava/lang/Object;JI)V",
        native_unsafe_put_int_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putIntUnaligned",
        "(Ljava/lang/Object;JIZ)V",
        native_unsafe_put_int_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getLongUnaligned",
        "(Ljava/lang/Object;J)J",
        native_unsafe_get_long_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getLongUnaligned",
        "(Ljava/lang/Object;JZ)J",
        native_unsafe_get_long_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putLongUnaligned",
        "(Ljava/lang/Object;JJ)V",
        native_unsafe_put_long_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putLongUnaligned",
        "(Ljava/lang/Object;JJZ)V",
        native_unsafe_put_long_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getShortUnaligned",
        "(Ljava/lang/Object;J)S",
        native_unsafe_get_short_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getShortUnaligned",
        "(Ljava/lang/Object;JZ)S",
        native_unsafe_get_short_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putShortUnaligned",
        "(Ljava/lang/Object;JS)V",
        native_unsafe_put_short_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putShortUnaligned",
        "(Ljava/lang/Object;JSZ)V",
        native_unsafe_put_short_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getCharUnaligned",
        "(Ljava/lang/Object;J)C",
        native_unsafe_get_char_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getCharUnaligned",
        "(Ljava/lang/Object;JZ)C",
        native_unsafe_get_char_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putCharUnaligned",
        "(Ljava/lang/Object;JC)V",
        native_unsafe_put_char_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "putCharUnaligned",
        "(Ljava/lang/Object;JCZ)V",
        native_unsafe_put_char_mb, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(u2, "storeFence", "()V", native_unsafe_fence, cratonvm_native_api::NativeKind::Bridge);
    r.register(u2, "loadFence", "()V", native_unsafe_fence, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        u2,
        "fullFence",
        "()V",
        native_unsafe_fence,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "park",
        "(ZJ)V",
        native_unsafe_park,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "unpark",
        "(Ljava/lang/Object;)V",
        native_unsafe_unpark,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
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
        native_unsafe_get_and_add_int, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getAndSetInt",
        "(Ljava/lang/Object;JI)I",
        native_unsafe_get_and_set_int, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getAndAddLong",
        "(Ljava/lang/Object;JJ)J",
        native_unsafe_get_and_add_long, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getAndSetLong",
        "(Ljava/lang/Object;JJ)J",
        native_unsafe_get_and_set_long, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "getAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
        native_unsafe_get_and_set_object, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "copyMemory",
        "(Ljava/lang/Object;JLjava/lang/Object;JJ)V",
        unsafe_natives::native_unsafe_copy_memory_consolidated, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(u2, "pageSize", "()I", native_unsafe_page_size, cratonvm_native_api::NativeKind::Bridge);
    r.register(u2, "addressSize", "()I", native_unsafe_address_size, cratonvm_native_api::NativeKind::Bridge);

    // --- Remaining Unsafe methods (Phase 44) ---

    // allocateMemory / reallocateMemory / freeMemory — off-heap memory, all
    // backed by the arena store (the allocate/realloc bodies delegate to the
    // consolidated natives, which `register_consolidated_off_heap_store`
    // registers over these later anyway).
    r.register(u, "allocateMemory", "(J)J", native_unsafe_allocate_memory, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        u,
        "reallocateMemory",
        "(JJ)J",
        native_unsafe_allocate_memory_realloc, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(u, "freeMemory", "(J)V", native_unsafe_free_memory_ext, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        u2,
        "allocateMemory0",
        "(J)J",
        native_unsafe_allocate_memory,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "reallocateMemory0",
        "(JJ)J",
        native_unsafe_allocate_memory_realloc,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
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
        native_unsafe_throw_exception, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
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
        native_unsafe_should_be_initialized, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
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
        unsafe_natives::native_unsafe_static_field_base, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "staticFieldBase",
        "(Ljava/lang/reflect/Field;)Ljava/lang/Object;",
        unsafe_natives::native_unsafe_static_field_base, cratonvm_native_api::NativeKind::Bridge,
    );

    // ensureClassInitialized — trigger class initialization
    r.register(
        u,
        "ensureClassInitialized",
        "(Ljava/lang/Class;)V",
        native_unsafe_ensure_class_initialized, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        u2,
        "ensureClassInitialized",
        "(Ljava/lang/Class;)V",
        native_unsafe_ensure_class_initialized, cratonvm_native_api::NativeKind::Bridge,
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
    r.register(cds_cls, "isDumpingClassList0", "()Z", native_return_false, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cds_cls,
        "isDumpingArchive0",
        "()Z",
        native_cds_is_dumping_archive0, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cds_cls,
        "isSharingEnabled0",
        "()Z",
        native_cds_is_sharing_enabled0, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cds_cls,
        "logLambdaFormInvoker",
        "(Ljava/lang/String;)V",
        native_noop,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cds_cls,
        "initializeFromArchive",
        "(Ljava/lang/Class;)V",
        native_noop,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
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
    r.register(
        cds_cls,
        "getRandomSeedForDumping",
        "()J",
        |_ctx, _args| Ok(Some(Value::Long(0))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cds_cls,
        "dumpClassList",
        "(Ljava/lang/String;)V",
        native_noop,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
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
    r.register(
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
    }

/// Extract the field offset (slot index) from args at the given position.
/// In our VM, offset = slot index (not byte offset).
pub fn unsafe_offset(args: &[Value], pos: usize) -> usize {
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
pub fn unsafe_array_index_from_offset(
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
pub fn unsafe_checked_array_index(
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
pub fn unsafe_obj(args: &[Value], pos: usize) -> Option<cratonvm_types::ObjectRef> {
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
pub const UNSAFE_BYTE_FIELD_OFFSET_BASE: usize = 16;

/// The `Unsafe` offset this VM answers for instance-field `slot`.
pub fn unsafe_encode_field_offset(byte_offsets: bool, slot: usize) -> usize {
    if byte_offsets {
        UNSAFE_BYTE_FIELD_OFFSET_BASE + slot * 8
    } else {
        slot
    }
}

/// `(slot, byte within the slot)` of an instance-field `offset`. An offset
/// below the base decodes to a huge slot every heap accessor bounds-checks
/// away (no field), never to slot 0.
pub fn unsafe_decode_field_offset(byte_offsets: bool, offset: usize) -> (usize, usize) {
    if !byte_offsets {
        return (offset, 0);
    }
    let rel = offset.wrapping_sub(UNSAFE_BYTE_FIELD_OFFSET_BASE);
    (rel >> 3, rel & 7)
}

/// [`unsafe_decode_field_offset`] for the field arm of a native whose base is
/// `obj`: the identity for an array, a synthetic (minted / static) offset, the
/// `Buffer.address` sentinel, and whenever this VM's switch is off.
pub fn unsafe_field_slot(
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
pub fn unsafe_primitive_width(desc: u8) -> Option<usize> {
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
pub fn unsafe_value_bits(v: Value) -> Option<u64> {
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
pub fn unsafe_value_of_bits(desc: u8, bits: u64) -> Option<Value> {
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
pub struct FieldLane {
    pub obj: ObjectRef,
    pub slot: usize,
    pub shift: u32,
    pub mask: u64,
    pub desc: u8,
    /// The field's own width in bytes.
    pub field_width: usize,
}

impl FieldLane {
    pub fn locate(
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
    pub fn load(&self, ctx: &dyn NativeContext) -> Option<u64> {
        let bits = unsafe_value_bits(ctx.get_field_volatile(self.obj, self.slot))?;
        Some((bits >> self.shift) & self.mask)
    }

    /// Replace the lane with `f(current)` (`None` = leave it), atomically
    /// against the field's other accesses. Returns the lane witnessed and
    /// whether `f` wrote. A slot whose CAS keeps failing on an unchanged value
    /// (a NaN payload the CAS compares by value) ends with the plain store
    /// after 65 536 attempts, as `CRATONVM_VH_RMW_STUCK_GUARD` does.
    pub fn rmw(
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
                // JIT round 21 (lane revvm; correctness, unconditional): the
                // plain store only for a slot whose bits did NOT change -- the
                // stuck CAS of the doc above. A CAS that failed because another
                // thread changed the slot is that thread's progress; storing
                // `new` (computed from the stale `cur`) over it lost its update
                // and reported a CAS success the field never held.
                if unsafe_value_bits(ctx.get_field_volatile(self.obj, self.slot)) == Some(bits) {
                    ctx.set_field(self.obj, self.slot, new);
                    return Some((cur, true));
                }
                attempts = 0;
            }
            if attempts % 1024 == 0 {
                std::thread::yield_now();
            }
        }
    }
}

/// A read of `width` bytes at byte `k` (`k != 0`) of instance-field `slot`:
/// the bytes, zero-extended; `0` for a reference field or past the field.
pub fn unsafe_sub_field_load(
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
pub fn unsafe_sub_field_store(
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
pub fn unsafe_field_byte_at(
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
pub fn unsafe_field_set_byte_at(
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
pub fn unsafe_field_fill_bytes(
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
pub const UNSAFE_BYTE_STATIC_OFFSET_BASE: usize = 0x3000_0000;

/// The offset this VM answers for the minted static offset `minted`: scaled
/// (and registered for `class_id`'s static `field_index`) under byte field
/// offsets, `minted` itself otherwise.
pub fn unsafe_static_offset_answer(
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
pub fn unsafe_static_sub_target(
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

/// The declared descriptor byte of `class_id`'s static `field_index`: a scan
/// of the class's fields by name (allocating; see [`unsafe_static_desc_at`],
/// which falls back to the stored value's kind).
pub fn unsafe_static_declared_desc(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    field_index: usize,
) -> Option<u8> {
    for f in ctx.declared_fields(class_id) {
        if f.is_static && ctx.static_field_index_by_name(class_id, &f.name) == Some(field_index) {
            return f.descriptor.as_bytes().first().copied();
        }
    }
    None
}

/// JIT round 21 (lane unsafe): the static's descriptor byte, memoised in its
/// registration row (`key_offset` is the offset the VM answered for the static:
/// the accessed offset minus the byte `k` inside it). The declared scan
/// (`declared_fields` + `static_field_index_by_name` per field, string
/// formatting and allocation) ran on EVERY sub-word static access, inside the
/// JDK's getAndAdd / getAndSet retry loops. Only a declared answer is kept; a
/// row that names another static (re-registered) is not touched.
pub fn unsafe_static_desc_at(
    ctx: &dyn NativeContext,
    key_offset: usize,
    class_id: ClassId,
    field_index: usize,
) -> Option<u8> {
    let key = (ctx.vm_identity(), key_offset);
    let names_it = |t: &UnsafeStaticFieldTarget| {
        t.class_id == class_id.as_u32() && t.field_index == field_index
    };
    // Dev keeps every registration of `(vm, offset)` (one row per class): the
    // memo is the row that names this static.
    let memo = lock_unsafe_vm_offset_shard(unsafe_static_field_targets(), key_offset)
        .get(&key)
        .and_then(|rows| rows.iter().rev().find(|t| names_it(t)).copied());
    if let Some(t) = memo.filter(|t| t.desc != 0 && names_it(t)) {
        return Some(t.desc);
    }
    let Some(desc) = unsafe_static_declared_desc(ctx, class_id, field_index) else {
        return unsafe_static_value_desc(ctx, class_id, field_index);
    };
    if let Some(rows) =
        lock_unsafe_vm_offset_shard(unsafe_static_field_targets(), key_offset).get_mut(&key)
    {
        if let Some(t) = rows.iter_mut().rev().find(|t| names_it(t)) {
            t.desc = desc;
        }
    }
    Some(desc)
}

/// The stored value's kind of `class_id`'s static `field_index` (the
/// fallback when no declaration names it).
pub fn unsafe_static_value_desc(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    field_index: usize,
) -> Option<u8> {
    match ctx.get_static_field(class_id, field_index) {
        Value::Int(_) => Some(b'I'),
        Value::Long(_) => Some(b'J'),
        Value::Float(_) => Some(b'F'),
        Value::Double(_) => Some(b'D'),
        _ => None,
    }
}

/// JIT round 21, lane atomics (STATICCAS21-2): tell the VM that the exact
/// `Unsafe` offset `offset` on a `staticFieldBase` mirror names `class_id`'s
/// static `field_index`, at the point a whole-static atomic is about to run on
/// it (`NativeSystemAccess::note_unsafe_static_atomic_target`). The VM then
/// lets compiled `Unsafe` RMW prefixes perform the same hardware atomic on the
/// static's payload word. Only while this module's own static atomics are that
/// hardware update (`CRATONVM_UNSAFE_STATIC_HW_RMW`): with it off they are a
/// read-then-write a compiled locked instruction would not be atomic with. The
/// descriptor is asked lazily (the VM asks it only for an offset it has not
/// published yet).
pub fn unsafe_note_static_atomic_target(
    ctx: &dyn NativeContext,
    offset: usize,
    class_id: ClassId,
    field_index: usize,
) {
    if !unsafe_static_hw_rmw_enabled() {
        return;
    }
    // JIT tier2 wave 4 (tag scas21; the R4 finding of the atm21 review): the
    // descriptor comes from the registration row's memo
    // ([`unsafe_static_desc_at`]), not from a fresh `declared_fields` scan.
    // The VM asks for it on every whole-static atomic of an offset it has not
    // settled (a `float` / `boolean` / sub-word / reference static never
    // publishes, so it never settles), and the scan allocates per field.
    ctx.note_unsafe_static_atomic_target(offset, class_id, field_index, &|| {
        unsafe_static_desc_at(ctx, offset, class_id, field_index)
    });
}

/// `(desc, shift, mask)` of `width` bytes at byte `k` of a primitive static,
/// clamped to the field; `None` for a reference or past the field.
pub fn unsafe_static_lane(
    ctx: &dyn NativeContext,
    key_offset: usize,
    class_id: ClassId,
    field_index: usize,
    k: usize,
    width: usize,
) -> Option<(u8, u32, u64)> {
    let desc = unsafe_static_desc_at(ctx, key_offset, class_id, field_index)?;
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

/// Replace `width` bytes at byte `k` of a static with `f(current)`,
/// atomically against the static's other accesses: a CAS loop on the whole
/// static (`compare_and_exchange_static_field`), as HotSpot's CAS on the
/// containing word. Returns the lane witnessed and whether `f` wrote.
///
/// JIT round 21 (lane staticcas; correctness, unconditional): this was a read
/// and a write, so two threads' sub-word `getAndAdd`s on one static both
/// "succeeded" from the same read. The bounded fallback is `FieldLane::rmw`'s
/// after 378a0d4896: the plain store only for a static whose bits did NOT
/// change across the failed attempts.
pub fn unsafe_static_lane_rmw(
    ctx: &mut dyn NativeContext,
    key_offset: usize,
    class_id: ClassId,
    field_index: usize,
    k: usize,
    width: usize,
    f: &dyn Fn(u64) -> Option<u64>,
) -> Option<(u64, bool)> {
    let (desc, shift, mask) =
        unsafe_static_lane(&*ctx, key_offset, class_id, field_index, k, width)?;
    let mut attempts = 0u32;
    loop {
        let old = ctx.get_static_field(class_id, field_index);
        let bits = unsafe_value_bits(old)?;
        let cur = (bits >> shift) & mask;
        let Some(upd) = f(cur) else {
            return Some((cur, false));
        };
        let new_bits = (bits & !(mask << shift)) | ((upd & mask) << shift);
        let new = unsafe_value_of_bits(desc, new_bits)?;
        if ctx
            .compare_and_exchange_static_field(class_id, field_index, old, new)
            .1
        {
            return Some((cur, true));
        }
        attempts += 1;
        if attempts >= 1 << 16 {
            // A CAS that failed because another thread changed the static is
            // that thread's progress; only a static whose bits did not change
            // (a CAS that cannot compare its own read) takes the plain store.
            if unsafe_value_bits(ctx.get_static_field(class_id, field_index)) == Some(bits) {
                ctx.set_static_field(class_id, field_index, new);
                return Some((cur, true));
            }
            attempts = 0;
        }
        if attempts % 1024 == 0 {
            std::thread::yield_now();
        }
    }
}

/// `a` and `b` are the same static value bit for bit (a `float` / `double`
/// by its bits, so a NaN equals itself; a reference by identity).
pub fn unsafe_static_value_same(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// JIT round 21 (lane staticcas; correctness, unconditional): replace a
/// whole static with `f(current)` (`None` = leave it) atomically against the
/// static's other accesses -- a CAS loop on
/// `NativeContext::compare_and_exchange_static_field`, the static form of
/// `vh_cas_rmw`. Returns the value witnessed and whether `f` wrote. Every
/// static `getAndAdd` / `getAndSet` / `getAndBitwise*` of the `Unsafe` and
/// `VarHandle` natives runs here; each was a read then a write, and lost
/// updates under contention. A static whose CAS keeps failing on an UNCHANGED
/// value (an untyped cell the fallback compare cannot match) ends with the
/// plain store after 65 536 attempts, as `FieldLane::rmw` does; a static that
/// changed is another thread's progress and is retried. Named
/// `unsafe_static_cas_rmw` since the 2026-10-26 re-land: dev's
/// [`unsafe_static_rmw`] (JIT round 23 wave 1, lane L4) is the hardware-first
/// form the `Unsafe` static arms take; this loop serves the round-21 callers
/// (the static `VarHandle` bodies), whose closures see the whole value.
pub fn unsafe_static_cas_rmw(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    field_index: usize,
    f: &dyn Fn(Value) -> Option<Value>,
) -> (Value, bool) {
    let mut cur = ctx.get_static_field(class_id, field_index);
    let mut attempts = 0u32;
    loop {
        let Some(new) = f(cur) else {
            return (cur, false);
        };
        let (witness, ok) = ctx.compare_and_exchange_static_field(class_id, field_index, cur, new);
        if ok {
            return (cur, true);
        }
        attempts += 1;
        if attempts >= 1 << 16 {
            if unsafe_static_value_same(ctx.get_static_field(class_id, field_index), cur) {
                ctx.set_static_field(class_id, field_index, new);
                return (cur, true);            }
            attempts = 0;
        }
        if attempts % 1024 == 0 {
            std::thread::yield_now();
        }
        cur = witness;
    }
}

/// The byte width a value written through `Unsafe` carries.
pub fn unsafe_value_width(v: Value) -> Option<usize> {
    match v {
        Value::Int(_) | Value::Float(_) => Some(4),
        Value::Long(_) | Value::Double(_) => Some(8),
        _ => None,
    }
}

/// `synthetic_get`'s answer for byte `k != 0` of a static (the bytes from `k`
/// on, low 32 bits: the natives' returns narrow it to the access), or `None`.
pub fn unsafe_static_sub_get(
    ctx: &dyn NativeContext,
    base: Option<ObjectRef>,
    offset: usize,
) -> Option<Value> {
    let (class_id, field_index, k) = unsafe_static_sub_target(ctx, base, offset)?;
    let bits = unsafe_static_lane(ctx, offset - k, class_id, field_index, k, 8).and_then(
        |(_, shift, mask)| {
            unsafe_value_bits(ctx.get_static_field(class_id, field_index))
                .map(|b| (b >> shift) & mask)
        },
    );
    // Cast: the low 32 bits of the zero-extended bytes.
    Some(Value::Int(bits.unwrap_or(0) as u32 as i32))
}

/// `synthetic_put`'s store of `val` at byte `k != 0` of a static (its own
/// width, clamped to the field). `true` when the offset was such a byte
/// (written, or nothing to write for a reference value).
pub fn unsafe_static_sub_put(
    ctx: &mut dyn NativeContext,
    base: Option<ObjectRef>,
    offset: usize,
    val: Value,
) -> bool {
    let Some((class_id, field_index, k)) = unsafe_static_sub_target(&*ctx, base, offset) else {
        return false;
    };
    if let (Some(width), Some(bits)) = (unsafe_value_width(val), unsafe_value_bits(val)) {
        let _ = unsafe_static_lane_rmw(ctx, offset - k, class_id, field_index, k, width, &|_| {
            Some(bits)
        });
    }
    true
}

/// JIT round 21 (lane unsafefix; correctness, unconditional): `(class, static
/// index, 0)` when `offset` is a static's OWN offset on the base `obj` (a
/// `staticFieldBase` mirror) and a store of `width` bytes is NARROWER than the
/// static -- HotSpot's store replaces only the static's low `width` bytes
/// there -- or as wide as a `byte` / `boolean` / `short` / `char` static (a
/// `putShort` into a `char` static: the 16 bits are re-read as a char, 45196,
/// not the short's -20340 left in the static's int payload). `None` for
/// anything else (no static at the offset, a reference static, an int-width
/// or wider store as wide as the static, a store wider than it), so the
/// caller keeps its whole-value path. The byte `k != 0` form is
/// [`unsafe_static_sub_target`].
pub fn unsafe_static_exact_narrow_target(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
    width: usize,
) -> Option<(ClassId, usize, usize)> {
    if width == 0 || width > 4 {
        return None;
    }
    let (class_id, field_index) = unsafe_static_field_target_for_base(ctx, obj, offset)?;
    let field_width =
        unsafe_primitive_width(unsafe_static_desc_at(ctx, offset, class_id, field_index)?)?;
    let narrower = width < field_width || (width == field_width && field_width <= 2);
    narrower.then_some((class_id, field_index, 0))
}

/// `synthetic_cas`'s CAS at byte `k != 0` of a static, by the width of
/// `expected`; `None` when the offset is not such a byte.
pub fn unsafe_static_sub_cas(
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
    let Some((_, _, mask)) = unsafe_static_lane(&*ctx, offset - k, class_id, field_index, k, width)
    else {
        return Some(false);
    };
    let ex = ex & mask;
    Some(
        unsafe_static_lane_rmw(ctx, offset - k, class_id, field_index, k, width, &|cur| {
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
pub fn unsafe_field_lane_atomic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: ArenaAtomic,
    width: usize,
) -> Option<Value> {
    let offset = unsafe_offset(args, 2);
    // JIT round 21 (lane unsafe): a STATIC field's lane (null or mirror base,
    // either offset encoding): a sub-word access, or an access narrower than
    // the static, compares and replaces only its own bytes. The whole-value
    // static CAS compared a char's zero-extended value with the sign-extended
    // short (and a byte / short inside an int static with the whole int), so
    // the JDK's getAndAdd / getAndSet retry loops spun forever once the
    // volatile sub-word reads answered the narrow value.
    if is_synthetic_offset(offset)
        && unsafe_obj(args, 1).map_or(true, |o| {
            ctx.heap_kind_of(o) != cratonvm_types::ObjectKind::Array
        })
    {
        return unsafe_static_lane_atomic(ctx, args, offset, op, width);
    }
    let obj = unsafe_obj(args, 1)?;
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
        || offset == BUFFER_ADDRESS_SENTINEL
    {
        return None;
    }
    let (slot, k) = unsafe_field_slot(&*ctx, obj, offset);
    // JIT round 21 (lane unsafe): with slot offsets too (the switch off), an
    // access narrower than its field is a lane (k is always 0 there); the
    // whole-slot int CAS compared a short with the whole int and the JDK's
    // sub-word retry loops spun. An offset past the fields keeps its path.
    if !ctx.unsafe_byte_field_offsets() && slot >= ctx.object_num_fields(obj) {
        return None;
    }
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

/// [`unsafe_field_lane_atomic`]'s static arm (JIT round 21, lane unsafe):
/// the `width`-byte lane at byte `k` of the static `offset` names (an exact
/// static offset is `k = 0`). `None` (the caller keeps its path) for an
/// offset that names no static, a reference static, a lane past the field,
/// and a whole-field int / long access (exact on the whole-value path).
pub fn unsafe_static_lane_atomic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    offset: usize,
    op: ArenaAtomic,
    width: usize,
) -> Option<Value> {
    let (class_id, field_index, k) = match unsafe_static_sub_target(&*ctx, None, offset) {
        Some(t) => t,
        None => {
            let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
            (class_id, field_index, 0)
        }
    };
    let field_width = unsafe_primitive_width(unsafe_static_desc_at(
        &*ctx,
        offset - k,
        class_id,
        field_index,
    )?)?;
    if k == 0 && width >= field_width && width > 2 {
        return None;
    }
    let (_, _, mask) = unsafe_static_lane(&*ctx, offset - k, class_id, field_index, k, width)?;
    let arg = |i: usize| -> u64 {
        // Casts: the operand's bits, masked to the lane.
        match args.get(i) {
            Some(Value::Int(v)) => *v as i64 as u64 & mask,
            Some(Value::Long(v)) => *v as u64 & mask,
            _ => 0,
        }
    };
    let (a, b) = (arg(3), arg(4));
    let (cur, ok) = match op {
        ArenaAtomic::Cas | ArenaAtomic::Cae => {
            unsafe_static_lane_rmw(ctx, offset - k, class_id, field_index, k, width, &|cur| {
                (cur == a).then_some(b)
            })?
        }
        ArenaAtomic::Add => {
            unsafe_static_lane_rmw(ctx, offset - k, class_id, field_index, k, width, &|cur| {
                Some(cur.wrapping_add(a) & mask)
            })?
        }
        ArenaAtomic::Set => {
            unsafe_static_lane_rmw(ctx, offset - k, class_id, field_index, k, width, &|_| {
                Some(a)
            })?
        }
    };
    // Casts: the witnessed bytes as the access's Java type.
    Some(match (op, width) {
        (ArenaAtomic::Cas, _) => Value::Int(i32::from(ok)),
        (_, 1) => Value::Int(cur as u8 as i8 as i32),
        (_, 2) => Value::Int(cur as u16 as i16 as i32),
        (_, 8) => Value::Long(cur as i64),
        _ => Value::Int(cur as u32 as i32),
    })
}

#[inline]
pub fn unsafe_shard_of<K: std::hash::Hash + ?Sized>(key: &K) -> usize {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    key.hash(&mut h);
    (h.finish() as usize) & UNSAFE_SHARD_MASK
}

#[inline]
pub fn new_unsafe_sharded_map<K, V>() -> UnsafeShardedMap<K, V> {
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
pub type UnsafeVmOffsetKey = (usize, usize);

/// Lock the shard of a `(vm_identity, offset)`-keyed sharded map that holds
/// `offset` (every VM's row for it). Index the guard with
/// `(ctx.vm_identity(), offset)`.
#[inline]
pub fn lock_unsafe_vm_offset_shard<V>(
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
pub fn unsafe_static_field_targets(
) -> &'static UnsafeShardedMap<UnsafeVmOffsetKey, UnsafeStaticFieldRows> {
    static T: std::sync::OnceLock<UnsafeShardedMap<UnsafeVmOffsetKey, UnsafeStaticFieldRows>> =
        std::sync::OnceLock::new();
    T.get_or_init(new_unsafe_sharded_map)
}

/// The classes registered under one `(vm, offset)`, latest last, one entry per
/// class id. See [`unsafe_static_field_targets`].
pub type UnsafeStaticFieldRows = Vec<UnsafeStaticFieldTarget>;

/// Register `offset` as `class_id`'s static field `field_index` in VM `vm`.
/// A class registered again moves to the end (the latest); another class of
/// the same name joins the row instead of replacing it (gci cl48).
pub fn remember_unsafe_static_field_offset(
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
        desc: 0,
    });
}

/// The latest registration of `(vm, offset)`, without cloning the row.
pub fn latest_static_field_row(key: UnsafeVmOffsetKey) -> Option<(UnsafeStaticFieldTarget, bool)> {
    let map = lock_unsafe_vm_offset_shard(unsafe_static_field_targets(), key.1);
    let rows = map.get(&key)?;
    Some((*rows.last()?, rows.len() > 1))
}

/// The registration of `(vm, offset)` for `class_id`, if that class has one.
pub fn static_field_row_of_class(
    key: UnsafeVmOffsetKey,
    class_id: ClassId,
) -> Option<UnsafeStaticFieldTarget> {
    let map = lock_unsafe_vm_offset_shard(unsafe_static_field_targets(), key.1);
    map.get(&key)?
        .iter()
        .rev()
        .find(|t| t.class_id == class_id.as_u32())
        .copied()
}

/// A static-field registration made WITHOUT a VM identity: the VM-less
/// `lib.rs::register_static_field_offset`, kept only until its one caller
/// (`vm/src/vm/vm_util.rs`'s `sun.misc.Unsafe` latch fix-up) passes its VM --
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// It records the class NAME beside the registering VM's id, and a reader in
/// any VM accepts the row only for a class of that name in ITS OWN VM (the
/// base mirror's class, or the recorded id when it names the same class
/// there). A static's index is a property of the class file, so it is the
/// same in every VM that loaded the same class; the id is not.
pub struct UnscopedStaticFieldTarget {
    pub class_name: std::sync::Arc<str>,
    pub class_id: u32,
    pub field_index: usize,
}

/// Whether any [`UnscopedStaticFieldTarget`] was ever recorded. Lets every
/// per-VM miss (the synthetic-offset hot path in `synthetic_get` / `_put` /
/// `_cas` is one) skip the second lock with one relaxed load.
pub static UNSCOPED_STATIC_TARGETS_USED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn unscoped_static_field_targets(// addr-keyed: not an address -- synthetic Unsafe static-field offsets.
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
pub fn remember_unscoped_static_field_offset(
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
pub fn unscoped_static_field_target(
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
pub fn forget_vm_unsafe_static_field_targets(vm: usize) {
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
pub fn ctx_caller_name(ctx: &mut dyn NativeContext) -> Option<String> {
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
pub fn note_unsafe_side_store_offset(
    ctx: &mut dyn NativeContext,
    offset: usize,
    site: u32,
    nargs: usize,
) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    if unsafe_arena_names_block(offset as i64)
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
pub fn unsafe_static_field_target(
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
pub fn unsafe_static_field_target_via_base(
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

pub fn unsafe_static_get(ctx: &dyn NativeContext, offset: usize) -> Option<Value> {
    let (class_id, field_index) = unsafe_static_field_target(ctx, offset)?;
    Some(ctx.get_static_field(class_id, field_index))
}

pub fn unsafe_static_put(ctx: &mut dyn NativeContext, offset: usize, value: Value) -> bool {
    let Some((class_id, field_index)) = unsafe_static_field_target(&*ctx, offset) else {
        return false;
    };
    ctx.set_static_field(class_id, field_index, value);
    true
}

/// JIT round 23 wave 1, lane L4 (`CRATONVM_UNSAFE_STATIC_HW_RMW`, default ON,
/// `0` restores the plain read-then-write): an `Unsafe` read-modify-write of a
/// registered STATIC field (a null base with a static offset, or a class
/// mirror base reaching `synthetic_cas`) as the VM's ONE hardware update of the
/// static cell (`NativeSystemAccess::static_field_hw_update`, the same
/// primitive the static `VarHandle` natives use since JIT round 22 wave 5b).
///
/// These paths were a bare `get_static_field` + `set_static_field` with NO
/// lock at all: two concurrent `U.getAndAddInt(ForkJoinPool.class, POOLIDS,
/// 1)` could both read the same value and both report success (a duplicated
/// pool id), and a CAS could overwrite a `putstatic` that landed between its
/// read and its write. HotSpot's are one `LOCK CMPXCHG` / `LOCK XADD`.
///
/// JIT round 23 wave 2, lane L4: when the hardware update declines
/// (`CRATONVM_HW_STATIC_CAS=0`, `CRATONVM_NO_STATICS_INDEX`, an unpublished
/// slot, a value whose kind is not the cell's), the read-modify-write runs
/// under the class mirror's monitor (`lang_invoke::vh_with_static_lock`, the
/// static `VarHandle` natives' fallback), re-asking the hardware update under
/// the lock first, so two `Unsafe` static RMWs -- and an `Unsafe` one and a
/// `VarHandle` one -- are atomic against each other on every shape. It was an
/// UNLOCKED read-then-write.
///
/// `f(current, live)` answers the value to store (`None`: store nothing); it
/// may run more than once and must be pure. `live` are the operands that may
/// hold references: they are pinned across the monitor entry (a GC point) and
/// `f` must read them from its `live` argument, never from a capture.
/// Answers `Some((witness, stored))`; `None` only with the switch off (the
/// caller keeps its old path, the A/B control).
pub fn unsafe_static_rmw(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    field_index: usize,
    live: &[Value],
    f: &dyn Fn(Value, &[Value]) -> Option<Value>,
) -> Option<(Value, bool)> {
    if !unsafe_static_hw_rmw_enabled() {
        return None;
    }
    if let Some(r) = ctx.static_field_hw_update(class_id, field_index, &mut |cur| f(cur, live)) {
        return Some(r);
    }
    Some(crate::lang_invoke::vh_with_static_lock(
        ctx,
        class_id,
        live,
        |ctx, live| {
            if let Some(r) =
                ctx.static_field_hw_update(class_id, field_index, &mut |cur| f(cur, live))
            {
                return r;
            }
            let cur = ctx.get_static_field(class_id, field_index);
            match f(cur, live) {
                Some(nv) => {
                    ctx.set_static_field(class_id, field_index, nv);
                    (cur, true)
                }
                None => (cur, false),
            }
        },
    ))
}

/// `CRATONVM_UNSAFE_STATIC_HW_RMW`, read once (the `MemoSlot` memo the
/// neighbouring native-builtins gates use, e.g.
/// `lang_class::annotation_jdk_semantics`), not on every static RMW.
pub fn unsafe_static_hw_rmw_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

pub fn unsafe_static_get_and_add_long(
    ctx: &mut dyn NativeContext,
    offset: usize,
    delta: i64,
) -> Option<i64> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    // JIT round 23 wave 1, lane L4: one hardware update first.
    if let Some((cur, _)) = unsafe_static_rmw(ctx, class_id, field_index, &[], &|cur, _| match cur {
        Value::Long(old) => Some(Value::Long(old.wrapping_add(delta))),
        _ => None,
    }) {
        return Some(if let Value::Long(old) = cur { old } else { 0 });
    }
    let Value::Long(old) = ctx.get_static_field(class_id, field_index) else {
        return Some(0);
    };
    ctx.set_static_field(class_id, field_index, Value::Long(old.wrapping_add(delta)));
    Some(old)
}

pub fn unsafe_static_get_and_set_long(
    ctx: &mut dyn NativeContext,
    offset: usize,
    value: i64,
) -> Option<i64> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    // JIT round 23 wave 1, lane L4: one hardware update first.
    if let Some((cur, _)) =
        unsafe_static_rmw(ctx, class_id, field_index, &[], &|_, _| Some(Value::Long(value)))
    {
        return Some(if let Value::Long(old) = cur { old } else { 0 });
    }
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

pub fn unsafe_static_get_and_add_int(
    ctx: &mut dyn NativeContext,
    offset: usize,
    delta: i32,
) -> Option<i32> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    // JIT round 23 wave 1, lane L4: one hardware update first.
    if let Some((cur, _)) = unsafe_static_rmw(ctx, class_id, field_index, &[], &|cur, _| match cur {
        Value::Int(old) => Some(Value::Int(old.wrapping_add(delta))),
        _ => None,
    }) {
        return Some(if let Value::Int(old) = cur { old } else { 0 });
    }
    let Value::Int(old) = ctx.get_static_field(class_id, field_index) else {
        return Some(0);
    };
    ctx.set_static_field(class_id, field_index, Value::Int(old.wrapping_add(delta)));
    Some(old)
}

pub fn unsafe_static_get_and_set_int(
    ctx: &mut dyn NativeContext,
    offset: usize,
    value: i32,
) -> Option<i32> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    // JIT round 23 wave 1, lane L4: one hardware update first.
    if let Some((cur, _)) =
        unsafe_static_rmw(ctx, class_id, field_index, &[], &|_, _| Some(Value::Int(value)))
    {
        return Some(if let Value::Int(old) = cur { old } else { 0 });
    }
    let old = match ctx.get_static_field(class_id, field_index) {
        Value::Int(old) => old,
        _ => 0,
    };
    ctx.set_static_field(class_id, field_index, Value::Int(value));
    Some(old)
}

pub fn unsafe_static_get_and_set_object(
    ctx: &mut dyn NativeContext,
    offset: usize,
    value: Option<ObjectRef>,
) -> Option<Option<ObjectRef>> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    // JIT round 23 wave 1, lane L4: one hardware update first.
    let live = [Value::Object(value)];
    if let Some((cur, _)) =
        unsafe_static_rmw(ctx, class_id, field_index, &live, &|_, live| Some(live[0]))
    {
        return Some(if let Value::Object(old) = cur { old } else { None });
    }
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
pub fn unsafe_static_cas(
    ctx: &mut dyn NativeContext,
    offset: usize,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    // JIT round 23 wave 1, lane L4: one hardware CAS first.
    let live = [expected, new_val];
    if let Some((_, ok)) = unsafe_static_rmw(ctx, class_id, field_index, &live, &|cur, live| {
        let nv = unsafe_retag_bits_like(&cur, live[1]);
        unsafe_cas_values_equal(cur, live[0]).then_some(nv)
    }) {
        return Some(ok);
    }
    let cur = ctx.get_static_field(class_id, field_index);
    // Wave 127 (lane L4): a bit-equal float / double slot keeps its tag.
    let new_val = unsafe_retag_bits_like(&cur, new_val);
    let ok = unsafe_cas_values_equal(cur, expected);
    if ok {
        ctx.set_static_field(class_id, field_index, new_val);
    }
    Some(ok)
}

pub fn unsafe_static_field_target_for_base(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
) -> Option<(ClassId, usize)> {
    unsafe_static_field_row_for_base(ctx, obj, offset)
        .map(|row| (ClassId::new(row.class_id), row.field_index))
}

/// Resolve the mirror and carry its cached descriptor to the publication caller.
pub fn unsafe_static_field_row_for_base(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    offset: usize,
) -> Option<UnsafeStaticFieldTarget> {
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
        return unscoped_static_field_target(ctx, offset, Some(mirror_class)).map(
            |(class_id, field_index)| UnsafeStaticFieldTarget {
                class_id: class_id.as_u32(),
                field_index,
                desc: 0,
            },
        );
    };
    let class_id = ClassId::new(target.class_id);
    if ctx.class_id_from_mirror(obj) == Some(class_id) {
        return Some(target);
    }
    let obj_class = ctx.class_id_of_object(obj);
    let is_class_mirror = ctx
        .class_name_of_id(obj_class)
        .map(|name| name == "java/lang/Class")
        .unwrap_or(false);
    if is_class_mirror {
        if let Value::Int(raw) = ctx.get_field(obj, 0) {
            if raw >= 0 && ClassId::new(raw as u32) == class_id {
                return Some(target);
            }
        }
    }
    // gci cl48: the base mirrors another class registered under this offset
    // (a same-named class of another loader): that class's own static.
    if !shared {
        return None;
    }
    let mirror_class = unsafe_base_mirror_class(ctx, obj)?;
    static_field_row_of_class(key, mirror_class)
}

/// The class `obj` mirrors, when `obj` is a `java.lang.Class` mirror: the same
/// two recognitions [`unsafe_static_field_target_for_base`] applies to a
/// registered target, without one to compare against.
pub fn unsafe_base_mirror_class(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<ClassId> {
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

pub fn unsafe_cas_values_equal(cur: Value, expected: Value) -> bool {
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
pub fn unsafe_int_bits_of(old: &Value) -> Option<i32> {
    match old {
        Value::Int(v) => Some(*v),
        Value::Float(f) => Some(f.to_bits() as i32),
        _ => None,
    }
}

/// The long-width twin of [`unsafe_int_bits_of`]: a `double` slot answers
/// its raw bits.
pub fn unsafe_long_bits_of(old: &Value) -> Option<i64> {
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
pub fn unsafe_retag_bits_like(cur: &Value, new_val: Value) -> Value {
    match (cur, new_val) {
        (Value::Float(_), Value::Int(n)) => Value::Float(f32::from_bits(n as u32)),
        (Value::Double(_), Value::Long(n)) => Value::Double(f64::from_bits(n as u64)),
        (_, v) => v,
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
pub fn unsafe_instantiation_exception(
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
    Err(cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/lang/InstantiationException",
        "()V",
        &[],
        || {
            RuntimeError::IllegalStateException {
                message: "InstantiationException: no instances of this type exist".to_string(),
            }
            .into()
        },
    ))
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
pub fn unsafe_caller_is_boot_path(ctx: &mut dyn NativeContext) -> bool {
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
pub fn unsafe_receiver_is_sun_misc(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
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
pub fn unsafe_bare_iae(ctx: &mut dyn NativeContext, fallback: &str) -> MethodCallFailed {
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/lang/IllegalArgumentException",
        "()V",
        &[],
        || {
            RuntimeError::IllegalArgumentException {
                message: fallback.to_string(),
            }
            .into()
        },
    )
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
pub fn sun_misc_field_refusal(
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

pub fn native_unsafe_object_field_offset(
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
            // By the Field's own declaring `ClassId`, not by name: a name lookup re-resolves the class
            // globally, which is ambiguous when two loaders define the same name (a Quarkus dev-mode
            // restart reloads Netty/JCTools) -- see `resolve_field_index_by_class_id`.
            let resolved = ctx.resolve_field_index_by_class_id(class_id, &field_name).or_else(|| {
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

pub fn native_unsafe_static_field_offset(
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
pub fn refuse_non_array_class(
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
        {
            (
                "java/lang/NoClassDefFoundError",
                1,
                "java/lang/InvalidClassException".to_string(),
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
pub fn unsafe_null_class_npe_message(legacy: &str) -> Option<String> {
    {
        None
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
pub fn native_unsafe_array_base_offset(
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
pub fn native_unsafe_array_index_scale(
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
pub enum ArenaAtomic {
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
pub fn unsafe_arena_atomic(
    ctx: &dyn NativeContext,
    args: &[Value],
    op: ArenaAtomic,
    width: usize,
) -> Option<Value> {
    use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering::SeqCst};
    if matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return None;
    }
    let offset = unsafe_offset(args, 2);
    let ptr = unsafe_arena_access_ptr(offset as i64, width)?;
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
        let step = |cur: u64| -> Option<u64> {
            match op {
                ArenaAtomic::Cas | ArenaAtomic::Cae => (cur == a).then_some(b),
                ArenaAtomic::Add => Some(cur.wrapping_add(a) & mask),
                ArenaAtomic::Set => Some(a),
            }
        };
        // JIT round 21 proposal ATOMICS21-4: a misaligned word is HotSpot's
        // `LOCK CMPXCHG` at that address on x86-64 (the array arm's
        // `misaligned_locked_update`), atomic against plain stores too.
        // SAFETY: `ptr` is good for `width` bytes (see above).
        if let Some((cur, ok)) = unsafe { ctx.misaligned_raw_atomic_update(ptr, width, &step) } {
            (cur & mask, ok)
        } else {
            // The fallback (another architecture, a switch off, a split lock
            // under `CRATONVM_UNSAFE_SPLIT_LOCK_CAS=0`): one process-wide lock
            // keeps the pair of accesses atomic against every other misaligned
            // atomic; plain stores are not synchronised with it.
            static MISALIGNED: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _g = MISALIGNED.lock().unwrap_or_else(|e| e.into_inner());
            let mut buf = [0u8; 8];
            // SAFETY: `ptr` is good for `width` bytes (see above).
            unsafe { std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), width) };
            let cur = u64::from_le_bytes(buf) & mask;
            let new = step(cur);
            // JIT round 21, lane atomics (UNSAFE21-3; correctness,
            // unconditional): only the bytes the update changes are written
            // back (see `unsafe_wide_raw_update`).
            if let Some(new) = new {
                if let Some((first, last)) = unsafe_changed_byte_span(cur, new, width) {
                    let nb = new.to_le_bytes();
                    // SAFETY: as above; `first ..= last` lies inside `0 .. width`.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            nb.as_ptr().add(first),
                            ptr.add(first),
                            last - first + 1,
                        )
                    };
                }
            }
            (cur, new.is_some())
        }
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
/// `compareAndSetInt` (under `--real-jdk` no native shadows them), and the
/// volatile loads read the private side store while the CAS now works on the
/// bytes: the loop's compare never matched and it spun forever
/// (`sun.misc.Unsafe.getAndAddInt(null, allocateMemory(..), 3)`).
pub fn unsafe_arena_volatile_load(args: &[Value], width: usize) -> Option<u64> {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
    if matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return None;
    }
    let ptr = unsafe_arena_access_ptr(unsafe_offset(args, 2) as i64, width)?;
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
pub fn unsafe_arena_volatile_store(args: &[Value], width: usize, raw: u64) -> bool {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
    if matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return false;
    }
    let Some(ptr) = unsafe_arena_access_ptr(unsafe_offset(args, 2) as i64, width) else {
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
pub fn native_unsafe_get_int_volatile_arena(
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
pub fn native_unsafe_put_int_volatile_arena(
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

