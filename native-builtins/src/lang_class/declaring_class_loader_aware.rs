#[allow(unused_imports)]
use super::*;

/// Isolating-loader carve-out for `Class.getEnclosingClass()`/
/// `getDeclaringClass()`. Returns `None` (falls through to the existing
/// global-lookup fast/slow paths, unchanged) unless `class_id` has a
/// registered defining loader (`defining_loader_for`) that's eligible per
/// `is_loader_aware_resolution_eligible`, AND `class_id` genuinely has an
/// `InnerClasses` self-entry naming an outer class. When both hold, resolves
/// the outer-class NAME by invoking that loader's OWN `loadClass()`
/// (mirrors `drive_defining_loader_load`'s re-entrant-call pattern in
/// `vm::runtime::interpreter`) so the returned `ClassId` is the SAME
/// loader's copy as `class_id` itself -- never a stale, different-loader
/// copy from CratonVM's flat global store.
pub fn declaring_class_loader_aware(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Option<cratonvm_types::ClassId> {
    let loader_obj = crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), class_id.as_u32())?;

    // Cheap short-circuit FIRST: if the VM's existing global-lookup answer
    // already belongs to the SAME defining loader as `class_id`, it's
    // already correct -- skip the eligibility check and the expensive
    // re-entrant `loadClass()` call below entirely. This keeps the fix's
    // cost near-zero for the overwhelming common case (a single loader, no
    // same-named-outer-class collision) and only pays for loader-driven
    // resolution when there's a genuine mismatch worth fixing.
    if let Some(existing) = ctx.declaring_class(class_id) {
        if let Some(existing_loader) =
            crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), existing.as_u32())
        {
            if existing_loader.as_ptr() == loader_obj.as_ptr() {
                return None;
            }
        }
        // `existing` has no registered loader (a built-in/app-loader class)
        // or a DIFFERENT registered loader than `class_id` -- a genuine
        // potential mismatch, worth checking further below.
    }

    if !crate::classloader::is_loader_aware_resolution_eligible(ctx, loader_obj) {
        return None;
    }
    let class_name = ctx.class_name_of_id(class_id)?;
    // The member rule of `NativeContext::declaring_class` (wave 16, lane
    // innerclasses: [`declaring_member_row_outer`]).
    let rows = ctx.inner_classes(class_id);
    let outer_class = member_row_outer_of(&*ctx, &rows, &class_name)?;
    // JIT round 21 wave 26, lane reserr (`CRATONVM_DECLARING_LOADER_AWARE_VIA_ENTRY`):
    // the outer class is the class's own `InnerClasses` entry, resolved with
    // `klass_at` on HotSpot, so through the recorded constant-pool door
    // ([`class_seen_by_referencing_class_checked`]): a failure is recorded
    // (JVMS 5.4.3) and the callers' own door then rethrows it without a
    // second `loadClass`. The bare `loadClass` below swallowed the failure,
    // so every `getDeclaringClass0` asked the loader again
    // (`R21Fz6LookupInLoad1`: `asks(Gone)` 2, 4, 1254 where HotSpot reads 1).
    if declaring_loader_aware_via_entry_enabled() && outer_resolution_errors_apply(&*ctx, class_id) {
        return class_seen_by_referencing_class_checked(ctx, class_id, &outer_class).ok();
    }
    let dotted = outer_class.replace('/', ".");
    match crate::classloader_real::load_class_named_as_the_vm(ctx, loader_obj, &dotted) {
        Ok(Some(Value::Object(Some(mirror_obj)))) => ctx.class_id_from_mirror(mirror_obj),
        _ => None,
    }
}

/// The refusal for a member class `inner` whose declaring class resolved to
/// `outer`, under the per-VM `CRATONVM_INNER_CLASSES_AGREEMENT` (interpreter
/// round i2 wave 59, lane L3): [`inner_classes_disagreement`]'s message as the
/// `IncompatibleClassChangeError` the natives raise. One predictable branch
/// per call when the two classes share a defining loader.
pub fn inner_classes_disagreement_refusal(
    ctx: &mut dyn NativeContext,
    inner: ClassId,
    outer: ClassId,
) -> Option<MethodCallFailed> {
    if !ctx.inner_classes_agreement() {
        return None;
    }
    let message = inner_classes_disagreement(ctx, inner, outer)?;
    Some(cratonvm_types::error::LinkageError::IncompatibleClassChangeError { message }.into())
}

/// [`inner_classes_disagreement`] for a class that names its own declaring
/// class (an `InnerClasses` self-entry with an outer class; since wave 16
/// named or not, [`declaring_member_row_outer`]), for the `Class` natives that reach the declaring class without
/// `getDeclaringClass0` (`getEnclosingClass`, `getSimpleName`,
/// `getCanonicalName`). `None` when the switch is off, the class is not a
/// member class, or the declaring class does not resolve.
pub fn member_class_inner_classes_disagreement(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
) -> Option<MethodCallFailed> {
    if !ctx.inner_classes_agreement() || is_lambda_proxy_id(class_id) {
        return None;
    }
    let outer =
        declaring_class_loader_aware(ctx, class_id).or_else(|| ctx.declaring_class(class_id))?;
    inner_classes_disagreement_refusal(ctx, class_id, outer)
}

/// `java/lang/Class.getSimpleBinaryName0()Ljava/lang/String;`
///
/// Returns the simple binary name of this class if it is an inner class
/// (the `inner_name` from the InnerClasses attribute), or null if it is
/// a top-level class or anonymous class.
pub fn native_class_get_simple_binary_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    let class_name = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };

    // Search inner_classes for an entry where inner_class == this class
    let inner_classes = ctx.inner_classes(class_id);
    for (inner_class, _outer_class, inner_name, _flags) in &inner_classes {
        if inner_class == &class_name {
            if inner_name.is_empty() {
                // Anonymous class вЂ” no simple binary name
                return Ok(Some(Value::Object(None)));
            }
            let name_obj = ctx.intern_string(inner_name);
            return Ok(Some(Value::Object(Some(name_obj))));
        }
    }

    // Not an inner class вЂ” return null
    Ok(Some(Value::Object(None)))
}

/// `java/lang/Class.getEnclosingMethod0()[Ljava/lang/Object;`
///
/// Returns a 3-element Object array `[enclosingClass, methodName, methodDescriptor]`
/// if this class is a local or anonymous class defined inside a method, or null.
pub fn native_class_get_enclosing_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    match ctx.enclosing_method(class_id) {
        Some((enc_class, method_name, method_desc)) => {
            // Resolve the enclosing class IN THE SAME LOADER as this class.
            // Under `--real-jdk` the public `getEnclosingClass()` is the real
            // bytecode, which reads THIS native, so a loader-blind name lookup
            // made every Groovy closure after the first `GroovyShell`
            // (`beans$_run_closure1` -> a `beans` that several loaders define)
            // answer "no enclosing class", and `Closure.getThisType()`'s
            // `GeneratedClosure.class.isAssignableFrom(enclosing)` then threw
            // `NullPointerException: Class.isAssignableFrom: argument is null`.
            //
            // JIT round 18 wave 11 (lane mhresid; MHLOADER18-4): HotSpot's
            // `JVM_GetEnclosingMethodInfo` resolves it with `klass_at` and
            // `CHECK_NULL`, so a failed resolution is thrown
            // (`CRATONVM_CLASS_OUTER_RESOLUTION_ERRORS`).
            let enc_class_id = match crate::lang_reflect::loader_scoped_enclosing_lookup_checked(
                ctx, class_id, &enc_class, true,
            )? {
                Some(id) => id,
                None => return Ok(Some(Value::Object(None))),
            };
            let enc_mirror = ctx.get_class_mirror(enc_class_id);

            // Build 3-element Object array: [Class, String, String]
            //
            // gc-common w20-f: the mirror was stored after the array's
            // allocation and both strings after their own, all through
            // pre-allocation addresses. One handle scope for the mirror and
            // the array.
            let mut scope = NativeHandleScope::new(ctx);
            let mirror_h = scope.root(enc_mirror);
            let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, 3);
            let arr_h = scope.root(arr);
            let enc_mirror: ObjectRef = scope.get(&mirror_h);
            scope.set_array_element(arr, 0, Value::Object(Some(enc_mirror)));

            if !method_name.is_empty() {
                let name_obj = scope.create_string(&method_name);
                let arr: ObjectRef = scope.get(&arr_h);
                scope.set_array_element(arr, 1, Value::Object(Some(name_obj)));
                let desc_obj = scope.create_string(&method_desc);
                let arr: ObjectRef = scope.get(&arr_h);
                scope.set_array_element(arr, 2, Value::Object(Some(desc_obj)));
            }
            // If method_name is empty, slots 1 and 2 stay null (class was defined
            // directly inside an initializer, not a named method).

            let arr: ObjectRef = scope.get(&arr_h);
            Ok(Some(Value::Object(Some(arr))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `java/lang/Class.getGenericSignature0()Ljava/lang/String;`
///
/// Returns the generic signature string from the Signature attribute,
/// or null if no generic signature is present.
pub fn native_class_get_generic_signature(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    match ctx.class_signature(class_id) {
        Some(sig) => {
            let sig_obj = ctx.create_string(&sig);
            Ok(Some(Value::Object(Some(sig_obj))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `java/lang/Class.getRawAnnotations()[B`
///
/// Returns the raw bytes of the RuntimeVisibleAnnotations attribute,
/// or an empty byte array if no annotations are present.
pub fn native_class_get_raw_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        // OpenJDK returns null (not empty array) when there are no annotations.
        // AnnotationParser.parseAnnotations checks for null and short-circuits;
        // an empty byte[] would cause BufferUnderflowException -> AnnotationFormatError.
        None => return Ok(Some(Value::Object(None))),
    };

    // SBR wave 5 (reflmeta): the VM now answers real bytes (it answered none
    // before). `--real-jdk` hands them to the real `AnnotationParser` -- most
    // importantly `AnnotationType`'s `@Retention` lookup, without which every
    // parsed member annotation reads as CLASS-retained and is dropped;
    // `--compatible` keeps its `null`, byte-for-byte.
    //
    // `--compatible` too under `CRATONVM_JDK_TYPE_ANNOTATIONS` (interpreter
    // round i2 wave 57, lane L3): the JDK's `TypeAnnotationParser` and
    // `TypeVariableImpl` keep only the annotations `AnnotationType` finds
    // RUNTIME-retained, which it reads here (measured: `null` made every
    // type annotation and type-parameter annotation answer empty).
    let bytes = if ctx.is_jdk_only() || ctx.jdk_type_annotations() {
        ctx.raw_annotations(class_id)
    } else {
        Vec::new()
    };
    if bytes.is_empty() {
        // Match OpenJDK semantics: return null when no RuntimeVisibleAnnotations attribute.
        Ok(Some(Value::Object(None)))
    } else {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    }
}

/// `java/lang/Class.getRawTypeAnnotations()[B`
///
/// Returns the raw bytes of the RuntimeVisibleTypeAnnotations attribute,
/// or null if none are present (matches OpenJDK; an empty byte[] would cause
/// BufferUnderflowException in AnnotationParser).
pub fn native_class_get_raw_type_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    // SBR wave 5 (reflmeta): as `getRawAnnotations` above -- real bytes under
    // `--real-jdk` (a class type parameter's annotations), `null` otherwise.
    // Interpreter round i2 wave 56, lane L3: under `--compatible` too once
    // its type variables are the JDK's `TypeVariableImpl`
    // (`CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`), whose `getAnnotations()`
    // parses these bytes; `null` answered `class X<@A T>`'s `T` with no
    // annotation (`L3W56TypeParameterAnnotations`).
    let bytes = if ctx.is_jdk_only() || ctx.compatible_reflect_type_impls() {
        ctx.raw_type_annotations(class_id)
    } else {
        Vec::new()
    };
    if bytes.is_empty() {
        Ok(Some(Value::Object(None)))
    } else {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    }
}

/// `java/lang/Class.getConstantPool()Ljdk/internal/reflect/ConstantPool;`
///
/// Returns a ConstantPool mirror object. In our implementation, we allocate
/// a synthetic object that holds a reference to the class ID. The JDK uses
/// this for annotation parsing вЂ” we return null for now and handle annotation
/// parsing through getRawAnnotations().
pub fn native_class_get_constant_pool(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // HotSpot's `JVM_GetClassConstantPool` answers `null` for a primitive or
    // an array class (no `InstanceKlass`, no pool); this VM handed out an
    // empty pool for them (interpreter round i5 wave 106, lane L3;
    // `L3W106ConstantPoolRefs` rows `cp-of-*`).
    if class_mirror_is_primitive(ctx, this) {
        return Ok(Some(Value::Object(None)));
    }
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    if ctx
        .class_name_arc_of_id(class_id)
        .is_some_and(|name| name.starts_with('['))
    {
        return Ok(Some(Value::Object(None)));
    }

    // Allocate a ConstantPool carrying two PRIVATE slots, appended ABOVE every
    // field the real class declares.
    //
    // W7-49 (2026-08-12): real `jdk.internal.reflect.ConstantPool` (JDK 25.0.3.9,
    // `javap -p`) declares exactly one instance field — `private final Object
    // constantPoolOop`. The old two-slot form wrote `Int(class_id)` into it and
    // put the mirror one slot past the end of the real layout. `constantPoolOop`
    // is a REFERENCE the collector scans as an oop, and a small `Int` sitting in
    // it is the `MethodHandles$Lookup`/`allowedModes` shape §5 of
    // natives-over-real-jdk-classes.md calls heap corruption, not a wrong answer.
    //
    // Safe to move: this file holds the ONLY mention of that class name in the
    // workspace — nothing reads slot 0 or 1 back, and the real JDK's own
    // `ConstantPool` accessors are `native` with no registration here, so no
    // reader of either layout exists to break. In synthetic-JDK mode the class is
    // a fabricated stub, `appended_slot_base_for_class` answers 0, and the
    // allocation and both writes are byte-identical to what they were.
    //
    // gc-common w20-f: the mirror is pinned across the allocation (slot 1 was
    // written with the mirror's pre-allocation address).
    let this_pin = ctx.pin_native_root(this);
    let cp_alloc = try_alloc_with_appended_slots(ctx, "jdk/internal/reflect/ConstantPool", 2);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let (cp_obj, base) = cp_alloc?;
    // Private slot 0 stores the class_id as Int for later lookups.
    ctx.set_field(cp_obj, base, Value::Int(class_id.as_u32() as i32));
    // Private slot 1: the Class mirror reference for getDeclaringClass()
    ctx.set_field(cp_obj, base + 1, Value::Object(Some(this)));
    // SBR wave 5 (reflmeta): the real field, as HotSpot sets it
    // (`reflect_ConstantPool::set_cp` stores the pool holder's MIRROR in
    // `constantPoolOop`). Every `ConstantPool` native below is handed this
    // field as its first argument and resolves the pool from it, so the real
    // `AnnotationParser` / `TypeAnnotationParser` can read the class's pool.
    ctx.set_field_by_name(cp_obj, "constantPoolOop", Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(cp_obj))))
}

/// The pool holder of a `jdk.internal.reflect.ConstantPool` native call:
/// `args[1]` is the `constantPoolOop` the Java side passes (the holder's
/// mirror, see [`native_class_get_constant_pool`]); a null one falls back to
/// the receiver's own field, as HotSpot's `reflect_ConstantPool::get_cp` reads
/// the receiver.
pub fn constant_pool_holder(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<ClassId, MethodCallFailed> {
    let mirror = match args.get(1) {
        Some(Value::Object(Some(m))) => Some(*m),
        _ => match args.first() {
            Some(Value::Object(Some(this))) => {
                match ctx.get_field_by_name(*this, "constantPoolOop") {
                    Value::Object(Some(m)) => Some(m),
                    _ => None,                }            }
            _ => None,
        },
    };
    match mirror.and_then(|m| mirror_class_id(ctx, m)) {
        Some(id) => Ok(id),
        None => Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some("constantPoolOop".to_string()),
        }
        .into()),
    }
}

/// HotSpot `jvm.cpp` `bounds_check` then the entry at `args[2]`.
pub fn constant_pool_checked_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<cratonvm_native_api::ReflectConstant, MethodCallFailed> {
    let holder = constant_pool_holder(ctx, args)?;
    let index = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => -1,
    };
    let size = ctx.reflect_constant_pool_size(holder).unwrap_or(0);
    if index < 0 || index as usize >= size {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Constant pool index out of bounds".to_string(),
            }
            .into(),
        );
    }
    Ok(ctx.reflect_constant_pool_entry(holder, index))
}

pub fn constant_pool_wrong_type() -> MethodCallFailed {
    cratonvm_types::error::RuntimeError::IllegalArgumentException {
        message: "Wrong type at constant pool index".to_string(),
    }
    .into()
}

/// `jdk.internal.reflect.ConstantPool.getSize0(Object)I` -- `constant_pool_count`.
pub fn native_constant_pool_get_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let holder = constant_pool_holder(ctx, args)?;
    let size = ctx.reflect_constant_pool_size(holder).unwrap_or(0);
    Ok(Some(Value::Int(i32::try_from(size).unwrap_or(i32::MAX))))
}

/// `ConstantPool.getUTF8At0(Object, int)String` -- the interned text of a
/// `CONSTANT_Utf8` entry (what `AnnotationParser` reads every type name,
/// element name, `String` value and class literal through).
pub fn native_constant_pool_get_utf8_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::Utf8(units) => {
            let text = String::from_utf16(&units);
            let s = match text {
                Ok(t) => ctx.create_string_uninterned(&t),
                Err(_) => ctx.create_string_from_units(&units),
            };
            Ok(Some(Value::Object(Some(s))))
        }
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getStringAt0(Object, int)String` -- a `CONSTANT_String`.
pub fn native_constant_pool_get_string_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::String(units) => {
            let text = String::from_utf16(&units);
            let s = match text {
                // HotSpot answers the interned `ldc` String here, so identity matters.
                Ok(t) => ctx.intern_string(&t),
                Err(_) => ctx.create_string_from_units(&units),
            };
            Ok(Some(Value::Object(Some(s))))
        }
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getIntAt0(Object, int)I`.
pub fn native_constant_pool_get_int_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::Integer(v) => Ok(Some(Value::Int(v))),
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getLongAt0(Object, int)J`.
pub fn native_constant_pool_get_long_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::Long(v) => Ok(Some(Value::Long(v))),
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getFloatAt0(Object, int)F`.
pub fn native_constant_pool_get_float_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::Float(v) => Ok(Some(Value::Float(v))),
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getDoubleAt0(Object, int)D`.
pub fn native_constant_pool_get_double_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::Double(v) => Ok(Some(Value::Double(v))),
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getTagAt0(Object, int)B` -- the JVMS tag; HotSpot answers
/// `JVM_CONSTANT_Class` (7) for a class entry whatever its resolution state
/// and 0 (`Invalid`) for index 0 and a long/double's second slot.
pub fn native_constant_pool_get_tag_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_native_api::ReflectConstant as R;
    let tag: i32 = match constant_pool_checked_entry(ctx, args)? {
        R::Invalid => 0,
        R::Utf8(_) => 1,
        R::Integer(_) => 3,
        R::Float(_) => 4,
        R::Long(_) => 5,
        R::Double(_) => 6,
        R::Class(_) => 7,
        R::String(_) => 8,
        R::MemberRef { tag, .. } | R::DynamicRef { tag, .. } => i32::from(tag),
        R::NameAndType { .. } => 12,
        R::Other(t) => i32::from(t),
    };
    Ok(Some(Value::Int(tag)))
}

/// A `String[]` of `parts` (fresh, uninterned Strings, as HotSpot's
/// `java_lang_String::create_from_symbol` makes them), for
/// `getMemberRefInfoAt0` / `getNameAndTypeRefInfoAt0`.
pub fn constant_pool_string_array(ctx: &mut dyn NativeContext, parts: &[&str]) -> MethodCallResult {
    let component = reflection_component_id(ctx, "java/lang/String");
    let mut scope = NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(component, parts.len());
    let arr_h = scope.root(arr);
    for (i, part) in parts.iter().enumerate() {
        let s = scope.create_string_uninterned(part);
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(s)));
    }
    let arr = scope.get(&arr_h);
    Ok(Some(Value::Object(Some(arr))))
}

/// `ConstantPool.getClassAt0(Object, int)Class` -- HotSpot's
/// `JVM_ConstantPoolGetClassAt`: a `CONSTANT_Class` entry, RESOLVED as the
/// pool holder's own bytecode resolves it (its loader; a failure is the
/// resolution error, `NoClassDefFoundError`), any other tag "Wrong type at
/// constant pool index". Unregistered before interpreter round i5 wave 106,
/// lane L3: `UnsatisfiedLinkError` (`L3W106ConstantPoolRefs`).
pub fn native_constant_pool_get_class_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let holder = constant_pool_holder(ctx, args)?;
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::Class(name) => {
            let id = constant_pool_resolve_class(ctx, holder, &name)?;
            let mirror = ctx.get_class_mirror(id);
            Ok(Some(Value::Object(Some(mirror))))
        }
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getClassAtIfLoaded0(Object, int)Class` -- HotSpot's
/// `JVM_ConstantPoolGetClassAtIfLoaded`: the class a `CONSTANT_Class` entry
/// names when the pool holder's loader already sees it, else `null`; nothing
/// is loaded (interpreter round i5 wave 106, lane L3).
pub fn native_constant_pool_get_class_at_if_loaded(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let holder = constant_pool_holder(ctx, args)?;
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::Class(name) => {
            match constant_pool_class_if_loaded(ctx, holder, &name) {
                Some(id) => {
                    let mirror = ctx.get_class_mirror(id);
                    Ok(Some(Value::Object(Some(mirror))))
                }
                None => Ok(Some(Value::Object(None))),
            }
        }
        _ => Err(constant_pool_wrong_type()),
    }
}

/// The class a pool's references resolve through: the holder itself, or, for
/// a lambda proxy (which has no class-store entry and no loader of its own
/// here), its host -- HotSpot defines the spun class as a hidden nestmate in
/// the host's loader and package, so every resolution and access verdict is
/// the host's (interpreter round i5 wave 107, lane L3).
pub fn constant_pool_referencing_class(ctx: &dyn NativeContext, holder: ClassId) -> ClassId {
    if !is_lambda_proxy_id(holder) {
        return holder;
    }
    lambda_proxy_host_class(ctx, holder).unwrap_or(holder)
}

/// A hidden class's own `this_class` entry: HotSpot holds it resolved to the
/// hidden class itself, which no name lookup finds (a lookup of the name
/// would load or find ANOTHER class of the same name). For a lambda proxy the
/// entry carries the hidden name `Host$$Lambda+0x...`
/// (`lambda_proxy::lambda_proxy_spun_class`); for a `Lookup.defineHiddenClass`
/// class it is the class file's own name, which the store keeps as the base
/// of its `/0x<n>` name (`L3W107HiddenClassOwnPool`).
pub fn constant_pool_names_own_class(ctx: &dyn NativeContext, holder: ClassId, name: &str) -> bool {
    if is_lambda_proxy_id(holder) {
        return name.contains("$$Lambda+0x");
    }
    ctx.class_name_arc_of_id(holder).is_some_and(|own| {
        hidden_name_suffix_split(&own).is_some_and(|(base, _)| base == name)
    })
}

/// HotSpot's `ConstantPool::klass_at` for a reflective reader: resolve `name`
/// as the holder's own bytecode would (its loader; a failure is the
/// resolution error), then `verify_constant_pool_resolve`
/// (`LinkResolver::check_klass_accessibility`: an inaccessible class is
/// `IllegalAccessError` "failed to access class X from class Y (...)", the
/// text the VM's own `CONSTANT_Class` door answers). Wave 106's `getClassAt0`
/// made no access check (interpreter round i5 wave 107, lane L3;
/// `L3W107ConstantPoolMembers` rows `class-at-inaccessible`).
pub fn constant_pool_resolve_class(
    ctx: &mut dyn NativeContext,
    holder: ClassId,
    name: &str,
) -> Result<ClassId, MethodCallFailed> {
    if constant_pool_names_own_class(&*ctx, holder, name) {
        return Ok(holder);
    }
    let referencing = constant_pool_referencing_class(&*ctx, holder);
    let id = ctx.class_id_by_name_via_referencing_class(referencing, name)?;
    if let Some(message) = ctx.class_access_denial_for_name(referencing, name) {
        return Err(cratonvm_types::error::LinkageError::IllegalAccessError { message }.into());
    }
    Ok(id)
}

/// HotSpot's `ConstantPool::klass_at_if_loaded`: the class the holder's
/// loader already knows by `name`, nothing loaded; `None` too when the access
/// check refuses it (HotSpot clears that exception and answers `null`).
pub fn constant_pool_class_if_loaded(
    ctx: &mut dyn NativeContext,
    holder: ClassId,
    name: &str,
) -> Option<ClassId> {
    if constant_pool_names_own_class(&*ctx, holder, name) {
        return Some(holder);
    }
    let referencing = constant_pool_referencing_class(&*ctx, holder);
    let id = ctx.class_id_seen_by_class_without_loading(name, referencing)?;
    if ctx.class_access_denial_for_name(referencing, name).is_some() {
        return None;
    }
    Some(id)
}

/// The `RuntimeException` HotSpot's `get_method_at_helper` /
/// `get_field_at_helper` throw when the resolved class has no such member.
pub fn constant_pool_lookup_failure(ctx: &mut dyn NativeContext, message: &str) -> MethodCallFailed {
    match crate::jboss_module_loader::alloc_single_message_exception(
        ctx,
        "java/lang/RuntimeException",
        1,
        message,
    ) {
        Ok(exception) => MethodCallFailed::ExceptionThrown(exception),
        Err(failed) => failed,
    }
}

/// HotSpot's `InstanceKlass::find_field(name, sig)`: the class's own fields,
/// then its direct superinterfaces depth first (`find_interface_field`: each
/// interface's own fields, then its superinterfaces), then the superclass
/// chain the same way. The answer's `declaring_class_id` is the class that
/// declares it.
pub fn constant_pool_find_field(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<FieldMetadata> {
    fn local(
        ctx: &dyn NativeContext,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Option<FieldMetadata> {
        declared_fields_with_aliases(ctx, class_id)
            .into_iter()
            .find(|f| f.name == name && f.descriptor == descriptor)
    }
    fn interface_field(
        ctx: &dyn NativeContext,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
        depth: usize,
    ) -> Option<FieldMetadata> {
        if depth > 64 {
            return None;
        }
        for interface in ctx.class_interfaces(class_id) {
            if let Some(found) = local(ctx, interface, name, descriptor) {
                return Some(found);
            }
            if let Some(found) = interface_field(ctx, interface, name, descriptor, depth + 1) {
                return Some(found);
            }
        }
        None
    }
    let mut current = Some(class_id);
    let mut hops = 0usize;
    while let Some(class) = current {
        if let Some(found) = local(ctx, class, name, descriptor) {
            return Some(found);
        }
        if let Some(found) = interface_field(ctx, class, name, descriptor, 0) {
            return Some(found);
        }
        hops += 1;
        if hops > 256 {
            return None;
        }
        current = ctx.superclass_of(class);    }
    None
}

/// The four member resolvers of `jdk.internal.reflect.ConstantPool`
/// (`JVM_ConstantPoolGetMethodAt`, `...MethodAtIfLoaded`, `...FieldAt`,
/// `...FieldAtIfLoaded`; jvm.cpp `get_method_at_helper` /
/// `get_field_at_helper`), unregistered (`UnsatisfiedLinkError`) before
/// interpreter round i5 wave 107, lane L3
/// (`i106-L3-constant-pool-member-resolvers-unregistered`,
/// `L3W107ConstantPoolMembers`):
///
/// - the bounds check ("Constant pool index out of bounds"), then the tag: a
///   `Methodref` / `InterfaceMethodref` for the method forms, a `Fieldref`
///   for the field forms, else "Wrong type at constant pool index" (both
///   `IllegalArgumentException`);
/// - the reference's class through the pool ([`constant_pool_resolve_class`],
///   its access check included); the `IfLoaded` forms answer `null` when it
///   is not loaded ([`constant_pool_class_if_loaded`]);
/// - a method is looked up in THAT class only (`InstanceKlass::find_method`,
///   static or not, `<clinit>` included), a field through the hierarchy
///   ([`constant_pool_find_field`]); a miss is `RuntimeException("Unable to
///   look up method in target class")` / `("... field ...")`;
/// - `<init>` comes back as a `Constructor` (`Reflection::new_constructor`),
///   any other method as a `Method` (`new_method`), a field as a `Field`
///   (`new_field`): each a fresh object built as the declared-member
///   listings build theirs, so modifiers, slot and annotations agree.
pub fn constant_pool_member_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    want_field: bool,
    force_resolution: bool,
) -> MethodCallResult {
    use cratonvm_native_api::ReflectConstant as R;
    let holder = constant_pool_holder(ctx, args)?;
    let (class_name, name, descriptor) = match constant_pool_checked_entry(ctx, args)? {
        R::MemberRef {
            tag,
            class_name,
            name,
            descriptor,
            ..
        } if (tag == 9) == want_field => (class_name, name, descriptor),
        _ => return Err(constant_pool_wrong_type()),
    };
    let class_id = if force_resolution {
        constant_pool_resolve_class(ctx, holder, &class_name)?
    } else {
        match constant_pool_class_if_loaded(ctx, holder, &class_name) {
            Some(id) => id,
            None => return Ok(Some(Value::Object(None))),
        }
    };
    if want_field {
        let Some(meta) = constant_pool_find_field(&*ctx, class_id, &name, &descriptor) else {
            return Err(constant_pool_lookup_failure(
                ctx,
                "Unable to look up field in target class",
            ));
        };
        // `Reflection::new_field` resolves the field's type
        // (`resolve_reflected_signature_types`, as the listing does).
        if ctx.reflection_loads_signature_types() {
            resolve_reflected_signature_types(
                ctx,
                meta.declaring_class_id,
                std::iter::once((meta.descriptor.as_str(), &[][..])),
            )?;
        }
        let field = create_field_object(ctx, &meta);
        return Ok(Some(Value::Object(Some(field))));
    }
    let meta = declared_methods_with_synthetic(&*ctx, class_id)
        .into_iter()
        .find(|m| m.name == name && m.descriptor == descriptor)
        .or_else(|| {
            // A lambda proxy's private constructor is modelled, not stored.
            (name == "<init>" && is_lambda_proxy_id(class_id))
                .then(|| lambda_proxy_constructor(&*ctx, class_id))
                .flatten()
                .filter(|m| m.descriptor == descriptor)
        });
    let Some(meta) = meta else {
        return Err(constant_pool_lookup_failure(
            ctx,
            "Unable to look up method in target class",
        ));
    };
    if ctx.reflection_loads_signature_types() {
        resolve_reflected_signature_types(
            ctx,
            class_id,
            std::iter::once((meta.descriptor.as_str(), meta.exceptions.as_slice())),
        )?;
    }
    let member = if meta.name == "<init>" {
        create_constructor_object(ctx, &meta)?
    } else {
        create_method_object(ctx, &meta)?
    };
    Ok(Some(Value::Object(Some(member))))
}

/// `ConstantPool.getMethodAt0(Object, int)Member` ([`constant_pool_member_at`]).
pub fn native_constant_pool_get_method_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    constant_pool_member_at(ctx, args, false, true)
}

/// `ConstantPool.getMethodAtIfLoaded0(Object, int)Member`
/// ([`constant_pool_member_at`]).
pub fn native_constant_pool_get_method_at_if_loaded(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    constant_pool_member_at(ctx, args, false, false)
}

/// `ConstantPool.getFieldAt0(Object, int)Field` ([`constant_pool_member_at`]).
pub fn native_constant_pool_get_field_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    constant_pool_member_at(ctx, args, true, true)
}

/// `ConstantPool.getFieldAtIfLoaded0(Object, int)Field`
/// ([`constant_pool_member_at`]).
pub fn native_constant_pool_get_field_at_if_loaded(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    constant_pool_member_at(ctx, args, true, false)
}

/// `ConstantPool.getClassRefIndexAt0(Object, int)I` -- a field or method
/// reference's `class_index` (`JVM_ConstantPoolGetClassRefIndexAt`;
/// interpreter round i5 wave 106, lane L3).
pub fn native_constant_pool_get_class_ref_index_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::MemberRef { class_index, .. } => {
            Ok(Some(Value::Int(i32::from(class_index))))
        }
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getNameAndTypeRefIndexAt0(Object, int)I` -- the
/// `name_and_type_index` of a field or method reference, an `invokedynamic`
/// or a dynamic constant (`JVM_ConstantPoolGetNameAndTypeRefIndexAt`;
/// interpreter round i5 wave 106, lane L3).
pub fn native_constant_pool_get_name_and_type_ref_index_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_native_api::ReflectConstant as R;
    match constant_pool_checked_entry(ctx, args)? {
        R::MemberRef {
            name_and_type_index,
            ..
        }
        | R::DynamicRef {
            name_and_type_index,
            ..
        } => Ok(Some(Value::Int(i32::from(name_and_type_index)))),
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getMemberRefInfoAt0(Object, int)String[]` -- `{class
/// internal name, member name, descriptor}` of a field or method reference
/// (`JVM_ConstantPoolGetMemberRefInfoAt`; interpreter round i5 wave 106,
/// lane L3).
pub fn native_constant_pool_get_member_ref_info_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::MemberRef {
            class_name,
            name,
            descriptor,
            ..
        } => constant_pool_string_array(
            ctx,
            &[class_name.as_str(), name.as_str(), descriptor.as_str()],
        ),
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `ConstantPool.getNameAndTypeRefInfoAt0(Object, int)String[]` -- `{name,
/// descriptor}` of a `CONSTANT_NameAndType`
/// (`JVM_ConstantPoolGetNameAndTypeRefInfoAt`; interpreter round i5 wave 106,
/// lane L3).
pub fn native_constant_pool_get_name_and_type_ref_info_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match constant_pool_checked_entry(ctx, args)? {
        cratonvm_native_api::ReflectConstant::NameAndType { name, descriptor } => {
            constant_pool_string_array(ctx, &[name.as_str(), descriptor.as_str()])
        }
        _ => Err(constant_pool_wrong_type()),
    }
}

/// `java/lang/Class.getDeclaredClasses0()[Ljava/lang/Class;`
///
/// Returns an array of Class mirrors for the immediate member classes /
/// interfaces declared inside this class (JLS В§8.5). Walks this class's
/// `InnerClasses` attribute (JVMS В§4.7.6) and selects entries whose
/// `outer_class_info` matches this class AND whose `inner_name` is non-empty
/// вЂ” the empty-`inner_name` case denotes an anonymous class, which `Class.
/// getDeclaredClasses` MUST exclude (HotSpot's `getDeclaredClasses0` checks
/// the same predicate).
///
/// Each surviving entry is then *resolved* through `load_class` so the inner
/// class actually has a `ClassId` (and therefore a mirror) вЂ” without this,
/// calling `getDeclaredClasses()` on a class whose members haven't been
/// referenced yet would always return an empty array, because
/// `class_id_by_name` only finds already-loaded classes. An unresolvable entry
/// throws its resolution error, as HotSpot's `klass_at(ioff, CHECK_NULL)` does
/// (`CRATONVM_CLASS_OUTER_RESOLUTION_ERRORS`; it used to be dropped, which the
/// old text here wrongly called HotSpot's behaviour).
pub fn native_class_get_declared_classes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = empty_class_array(ctx);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let inner_classes = ctx.inner_classes(class_id);

    // Collect inner classes where outer_class == this class AND inner_name is
    // non-empty (anonymous classes have inner_name_index == 0, which the
    // parser surfaces as an empty string).
    // gen r4w3/rooting: accumulate `ClassId`s, not mirrors — later iterations
    // run `loadClass` / `load_class` (GC points) and mirrors move. Mirrors are
    // materialised after the loop.
    let mut declared: Vec<ClassId> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Interpreter round i5 wave 101, lane L3 (`L3W101DeclaredClassesEntries`):
    // HotSpot's `JVM_GetDeclaredClasses` takes EVERY entry whose
    // `inner_class_info` and `outer_class_info` are both set and whose outer
    // names this class -- it does not look at `inner_name` (an anonymous or
    // local class is excluded because its `outer_class_info` is 0), does not
    // drop a repeated entry, and does not skip an entry naming the class
    // itself. `CRATONVM_DECLARED_CLASSES_HOTSPOT_ENTRIES=0` restores the old
    // filter (name set, first entry per member, never itself).
    let hotspot_entries = reflect_door_caches(&*ctx).is_none_or(|c| c.declared_classes_entries);
    for (inner_class, outer_class, inner_name, _flags) in &inner_classes {
        if outer_class != &class_name || inner_class.is_empty() {
            continue;
        }
        if !hotspot_entries {
            if inner_class == &class_name || inner_name.is_empty() {
                // Anonymous (and most local) classes.
                continue;
            }
            if !seen.insert(inner_class.clone()) {
                continue;
            }
        }
        // Resolve the inner class. Prefer the already-loaded id; otherwise
        // ask the VM to load it (without initializing вЂ” `load_class` calls
        // `load_class_concurrent`, which stops before <clinit>). Failures are
        // dropped, matching HotSpot's behaviour for missing inner classes.
        // Use `class_id_by_name_near(inner_class, class_id)` rather than a
        // plain by-name lookup: a nested class sharing its outer class's
        // simple name across two loaders (e.g. Spring's
        // `@CompileWithForkedClassLoader` re-defining an outer AND all its
        // nested `@Configuration` classes under a fresh forked loader,
        // alongside the original app-loader copies) must resolve to the
        // SAME loader's copy as `this` outer class, not whichever loader's
        // copy happens to sit first in the global flat lookup. Otherwise
        // `getDeclaredClasses()` on the forked outer class returns the
        // app-loader's nested classes, splitting identity for anything
        // downstream that reads their annotations/enum constants (e.g. a
        // `@Conditional` enum attribute compared by `==` against a value
        // resolved through the forked loader elsewhere).
        let loader_id = ctx.loader_id_of_class(class_id);
        let exact_inner = if loader_id >= 3 {
            ctx.class_id_defined_by_loader_exact(inner_class, loader_id as u32)
        } else {
            None
        };
        let inner_id = match exact_inner {
            Some(id) => Some(id),
            None => {
                // Not already loaded under the outer class's own loader.
                // Drive that loader's `loadClass` DIRECTLY (JVMS §5.4.3
                // initiating-loader semantics) before falling back to the
                // global loader-blind `load_class` — mirrors
                // `drive_defining_loader_load` in `vm/src/runtime/
                // interpreter.rs`, unavailable here (crate-boundary), so
                // reimplemented locally against `defining_loader_for`.
                // Without this, a nested class that the outer class's
                // loader has never been asked to load (e.g. Spring's
                // `@CompileWithForkedClassLoader` outer config class gets
                // its own fresh copy via a `ldc`, but nothing ever calls
                // `forkedLoader.loadClass("...NestedConfig")` directly)
                // falls straight to the global lookup and silently returns
                // the FIRST same-named class some other loader registered.
                //
                // `defining_loader_for` is narrowly populated (only the ~4
                // explicit `register_defining_loader` call sites write it),
                // so a class defined via the ordinary isolated-`URLClassLoader`
                // native path (`ucl_try_define_local_class`) can have a
                // perfectly good `UserDefined` namespace id in `class_manager`
                // (readable via `loader_id_of_class`) while still missing
                // from that side table. Fall back to the namespace-id-keyed
                // reverse lookup (`loader_object_for_namespace_id_in`, backed by
                // the SAME object-keyed store `loader_namespace_id` itself
                // writes) before giving up on driving the loader directly —
                // this is exactly the scenario a JUnit5 `@Nested` class run
                // under Spring Boot's `ModifiedClassPathClassLoader` fork
                // hits: `getDeclaredClasses()` is called on a freshly
                // isolated outer `Class` whose defining loader was never
                // separately registered.
                let loader_obj =
                    crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), class_id.as_u32())
                        .or_else(|| {
                            let ns_id = ctx.loader_id_of_class(class_id);
                            (ns_id >= 3)
                                .then(|| {
                                    crate::classloader::loader_object_for_namespace_id_in(
                                        crate::classloader::loader_tables(ctx),
                                        ns_id as u32,
                                    )
                                })
                                .flatten()
                        });
                let had_loader = loader_obj.is_some();
                let driven = loader_obj.and_then(|loader_obj| {
                    let dotted = inner_class.replace('/', ".");
                    match crate::classloader_real::load_class_named_as_the_vm(
                        ctx, loader_obj, &dotted,
                    ) {
                        Ok(Some(Value::Object(Some(mirror)))) => mirror_class_id(ctx, mirror),
                        _ => None,
                    }
                });
                // Interpreter round i4 wave 84, lane L3: HotSpot's
                // `JVM_GetDeclaredClasses` resolves every member entry with
                // `cp->klass_at(ioff, CHECK_NULL)`, so a member class the outer
                // class's own loader cannot load is that resolution's
                // `NoClassDefFoundError`, thrown out of `getDeclaredClasses()`.
                // CratonVM dropped the entry (and, for a user-defined loader
                // that refused it, could answer another loader's copy through
                // the loader-blind fallback below). Same switch and scope as
                // the outer-class lookups (`outer_resolution_errors_apply`).
                let throws = outer_resolution_errors_apply(ctx, class_id);
                match driven {
                    Some(id) => Some(id),
                    None if (had_loader || loader_id >= 3) && throws => Some(
                        class_seen_by_referencing_class_checked(ctx, class_id, inner_class)?,
                    ),
                    None => {
                        let found = ctx.class_id_by_name_near(inner_class, class_id).or_else(|| {
                            ctx.load_class(inner_class)
                                .ok()
                                .flatten()
                                .and_then(|value| match value {
                                    Value::Object(Some(mirror)) => mirror_class_id(ctx, mirror),
                                    _ => None,
                                })
                        });
                        match found {
                            Some(id) => Some(id),
                            None if throws => Some(class_seen_by_referencing_class_checked(
                                ctx,
                                class_id,
                                inner_class,
                            )?),
                            None => None,
                        }
                    }
                }
            }
        };
        if let Some(inner_id) = inner_id {
            declared.push(inner_id);
        }
    }

    // `Class[]`, the declared type (i6-L2).
    let class_comp = class_component_id(ctx);
    let arr = ctx.new_ref_array(class_comp, declared.len());
    // gen r4w3/rooting: same shape as `native_class_get_nest_members` —
    // `get_class_mirror` may allocate on first use, so keep the array pinned.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, inner_id) in declared.iter().enumerate() {
        let mirror = ctx.get_class_mirror(*inner_id);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(arr))))
}

/// `java/lang/Class.getNestHost0()Ljava/lang/Class;`
///
/// Returns the nest host of this class. If this class has a NestHost attribute,
/// returns that class; otherwise returns itself (every class is its own nest host).
pub fn native_class_get_nest_host(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(Some(this)))),
    };

    // Lambda proxies: HotSpot reports the *defining* class as the nest host
    // (e.g. `Refl6` for a lambda defined in Refl6), not the synthetic proxy
    // itself. Resolve the host; fall through to self if it can't be resolved.
    // bug-06 fam5 #1.
    if is_lambda_proxy_id(class_id) {
        // Round 18 wave 9, lane mhloader: the host as the proxy's own
        // namespace sees it first (two loaders may define the host's name),
        // then the flat lookup. Interpreter round i5 wave 100, lane L3: and
        // that defining class's own nest host, as HotSpot gives a NESTMATE
        // hidden class (`lambda_proxy_nest_host`).
        if let Some(host_id) = lambda_proxy_nest_host(ctx, class_id) {
            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(host_id)))));
        }
    }

    match ctx.nest_host_name(class_id) {
        Some(host_name) => {
            // JVMS 5.4.4: the nest host is resolved through the SAME defining
            // loader as the member. A by-name global lookup answers `None` when
            // two user-defined loaders each hold their own copy of the host's
            // name (Spring Boot's `ModifiedClassPathClassLoader` forks beside the
            // application loader), and the `None` below then made a nest member
            // its OWN host -- so a retransformation rebuilt from
            // `Class.getNestHost()` (ByteBuddy, under Mockito's inline mock maker)
            // dropped the `NestHost` attribute and was refused with
            // `attempted to change the class NestHost` (`SpringApplicationTests`
            // `withHookWhenHookThrowsAbandonedRunExceptionAbandonsRun`).
            // `getNestMembers0` already resolves the same way.
            let mut loader_obj = None;
            let mut loader_resolved = false;
            // The resolution may run `loadClass` (a GC point) and `this` is
            // the answer when the host does not validate: pinned and re-read
            // (JIT round 20 wave 16, lane nest), as `getNestMembers0` does.
            let this_pin = ctx.pin_native_root(this);
            let host_id = resolve_nestmate_via_defining_loader(
                ctx,
                args,
                &host_name,
                &mut loader_obj,
                &mut loader_resolved,
            );
            // JIT round 18 wave 9, lane mhloader: HotSpot's
            // `InstanceKlass::nest_host` validation (`CRATONVM_CLASS_NEST_HOST_VALIDATE`).
            let host_id = host_id.filter(|&h| nest_host_is_valid(ctx, class_id, h));
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            match host_id {
                Some(host_id) => {
                    let mirror = ctx.get_class_mirror(host_id);
                    Ok(Some(Value::Object(Some(mirror))))
                }
                // If the host class can't be resolved, JDK spec says return self
                None => Ok(Some(Value::Object(Some(this)))),
            }
        }
        // No NestHost attribute вЂ” this class is its own nest host
        None => Ok(Some(Value::Object(Some(this)))),
    }
}

/// `CRATONVM_CLASS_NEST_HOST_VALIDATE` -- correctness, default ON, `=0`
/// answers the resolved `NestHost` without HotSpot's validation (JIT round 18
/// wave 9, lane mhloader). See [`nest_host_is_valid`].
pub fn class_nest_host_validate_enabled() -> bool {
    true
}

/// The internal package prefix of an internal class name (`""` for the
/// unnamed package).
pub fn internal_package_of(name: &str) -> &str {
    name.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

/// Do `a` and `b` have the same defining loader? The class manager's loader
/// id first; when the ids differ, the defining-loader side table must also
/// disagree (the class manager's id can lag it for a forked loader).
pub fn same_defining_loader(
    ctx: &mut dyn NativeContext,
    a: cratonvm_types::ClassId,
    b: cratonvm_types::ClassId,
) -> bool {
    if ctx.loader_id_of_class(a) == ctx.loader_id_of_class(b) {
        return true;
    }
    let tables = crate::classloader::loader_tables(ctx);
    match (
        crate::classloader::defining_loader_for(tables, a.as_u32()),
        crate::classloader::defining_loader_for(tables, b.as_u32()),
    ) {
        (Some(x), Some(y)) => x.as_ptr() == y.as_ptr(),
        _ => false,
    }
}

/// HotSpot's `InstanceKlass::nest_host` validation of the class `host` that
/// `member`'s `NestHost` attribute resolved to (JIT round 18 wave 9, lane
/// mhloader): a valid host is in `member`'s run-time package (same defining
/// loader, same package name; "JVMS 5.4.4 indicates package check comes
/// first") and lists `member` in its `NestMembers`; otherwise `member` is its
/// own nest host. A child-first loader that defines `Outer$Inner` but not
/// `Outer` therefore gets `Outer$Inner.getNestHost() == Outer$Inner`. The
/// VM's own access checks already decide nestmates this way
/// (`classloading::access_control::confirmed_nest_host`); this makes the
/// reflective answer agree with them. Hidden classes are exempt (their host is
/// the defining `Lookup`'s, not a class-file claim). `true` with the switch
/// off.
/// The nest host of lambda proxy `class_id`: HotSpot's `JVM_LookupDefineClass`
/// gives a NESTMATE hidden class the lookup class's own NEST HOST
/// (`InstanceKlass::nest_host` of the lookup class), so a lambda defined in
/// `Outer$Inner` belongs to `Outer`'s nest. The proxy's recorded host is the
/// defining (lookup) class; its validated `NestHost` is followed one step.
/// `None` when the proxy records no host or the host is not loaded.
/// Interpreter round i5 wave 100, lane L3 (`L3W100LambdaNestMembers`).
/// `CRATONVM_LAMBDA_NEST_HOST_BY_ID` (parity, default on, `=0` off;
/// interpreter round i7 wave 142, lane L3): see [`lambda_proxy_nest_host`].
pub fn lambda_nest_host_by_id() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

pub fn lambda_proxy_nest_host(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Option<cratonvm_types::ClassId> {
    // Interpreter round i7 wave 142, lane L3 (`L3W141ReflectHunt` row
    // `hidden-lambda`, w139 run): the recorded defining class by id first.
    // By name, a host whose name a second loader also defines (the probe
    // loads its own class through two `URLClassLoader`s) was ambiguous, the
    // near lookup cannot place a synthetic proxy id in a namespace, and the
    // proxy answered ITSELF as its nest host where HotSpot answers the
    // lookup class's nest host. `CRATONVM_LAMBDA_NEST_HOST_BY_ID=0` restores
    // the by-name lookup only.
    let by_id = if lambda_nest_host_by_id() {
        ctx.lambda_proxy_host_id(class_id)
    } else {
        None
    };
    let lookup = match by_id {
        Some(id) => id,
        None => {
            let host = ctx.lambda_proxy_host(class_id)?;
            let near = if class_outer_via_own_loader_enabled() {
                ctx.class_id_by_name_near(&host, class_id)
            } else {
                None
            };
            near.or_else(|| ctx.class_id_by_name(&host))?
        }
    };
    let Some(outer_name) = ctx.nest_host_name(lookup) else {
        return Some(lookup);
    };
    let outer = ctx
        .class_id_by_name_near(&outer_name, lookup)
        .or_else(|| ctx.class_id_by_name(&outer_name))
        .filter(|&h| nest_host_is_valid(ctx, lookup, h));
    Some(outer.unwrap_or(lookup))
}

pub fn nest_host_is_valid(
    ctx: &mut dyn NativeContext,
    member: cratonvm_types::ClassId,
    host: cratonvm_types::ClassId,
) -> bool {
    if member == host || !class_nest_host_validate_enabled() || ctx.is_class_hidden(member) {
        return true;
    }
    let (Some(member_name), Some(host_name)) =
        (ctx.class_name_of_id(member), ctx.class_name_of_id(host))
    else {
        return true;
    };
    if internal_package_of(&member_name) != internal_package_of(&host_name)
        || !same_defining_loader(ctx, member, host)
    {
        return false;
    }
    ctx.nest_member_names(host)
        .iter()
        .any(|m| *m == member_name)
}

/// HotSpot `JVM_GetNestMembers`'s per-member filter: the member resolved from
/// `host`'s `NestMembers` is kept only when its own nest host is `host` (its
/// `NestHost` attribute names `host`, and [`nest_host_is_valid`] accepts the
/// pair). `true` with the switch off.
pub fn nest_member_is_valid(
    ctx: &mut dyn NativeContext,
    member: cratonvm_types::ClassId,
    host: cratonvm_types::ClassId,
) -> bool {
    if !class_nest_host_validate_enabled() || ctx.is_class_hidden(member) {
        return true;
    }
    let Some(host_name) = ctx.class_name_of_id(host) else {
        return true;
    };
    if ctx.nest_host_name(member).as_deref() != Some(host_name.as_str()) {
        return false;
    }
    nest_host_is_valid(ctx, member, host)
}

/// `java/lang/Class.getNestMembers0()[Ljava/lang/Class;`
///
/// Returns the nest members of this class. Per JVMS/javadoc, only the nest
/// *host* class file carries the `NestMembers` attribute — a nest *member*
/// only carries a `NestHost` attribute pointing back to it. So a class that
/// is itself a member (not the host) must resolve to its host first and
/// query the host's `NestMembers` list; querying the member's own class_id
/// directly always looks empty and silently degenerates to the "singleton
/// nest" fallback below, even for classes that do belong to a larger nest.
/// The returned array always includes `this` (JDK contract), plus the host
/// and every other resolved member.
/// Resolve `name` through `this_args[0]`'s own defining `ClassLoader`
/// FIRST, falling back to the global loader-blind `class_id_by_name` only
/// when no Java-level loader object is available (bootstrap-loaded `this`)
/// or the loader invocation fails.
///
/// **Why loader-first, not loader-as-fallback:** the previous ordering
/// (global lookup first, own-loader `loadClass` only on a miss) silently
/// accepted WHICHEVER loader in the process happened to already have a
/// same-named class loaded — correct for the overwhelmingly common
/// single-loader-per-name case, but wrong under any isolating/forked test
/// classloader (e.g. JUnit5's `ModifiedClassPathExtension`'s
/// `ModifiedClassPathClassLoader`, or Spring's `@ClassPathExclusions`).
/// There, a same-named class legitimately exists as two distinct,
/// reference-INEQUAL `Class` objects — one per loader — and JVMS requires
/// nest members / permitted subclasses to resolve relative to the SAME
/// defining loader as the nest host / sealed class asking. Silently
/// handing back an unrelated loader's copy breaks identity-sensitive
/// callers downstream (`HashMap<Class<?>, V>` keyed by one of these
/// classes, `==` comparisons) with no visible error — exactly the
/// `DialectOverridesAnnotationHelper.OVERRIDE_MAP` miss this was found
/// through (see the redis-hangs known-issues doc's residual section).
///
/// This isn't a fast/slow-path split: `ClassLoader.loadClass` itself
/// checks `findLoadedClass` before delegating/defining, so calling it on
/// an already-loaded class is cheap — always asking "the right loader"
/// costs no more than asking "any loader" did.
pub fn resolve_nestmate_via_defining_loader(
    ctx: &mut dyn NativeContext,
    this_args: &[Value],
    name: &str,
    loader_obj: &mut Option<ObjectRef>,
    loader_resolved: &mut bool,
) -> Option<ClassId> {
    if !*loader_resolved {
        *loader_resolved = true;
        *loader_obj = match native_class_get_class_loader(ctx, this_args) {
            Ok(Some(Value::Object(Some(l)))) => Some(l),
            _ => None,
        };
    }
    // gen r4w3/rooting: the cached `loader_obj` is reused by the caller's
    // NEXT call, but this call runs `loadClass` / `load_class` (GC points).
    // Pin it across the whole resolution and write the current address back.
    let pinned = loader_obj.map(|l| (ctx.pin_native_root(l), l));
    let resolved = resolve_nestmate_via_loader_inner(ctx, name, *loader_obj);
    if let Some((pin, l)) = pinned {
        *loader_obj = Some(ctx.read_native_pin(pin, l));
        ctx.unpin_native_roots(pin);
    }
    resolved
}

/// Body of [`resolve_nestmate_via_defining_loader`] once the loader object is
/// known; split out so the caller can keep that loader pinned across every
/// GC point in here (gen r4w3/rooting).
pub fn resolve_nestmate_via_loader_inner(
    ctx: &mut dyn NativeContext,
    name: &str,
    loader_obj: Option<ObjectRef>,
) -> Option<ClassId> {
    if let Some(loader) = loader_obj {
        let dotted = name.replace('/', ".");
        if let Ok(Some(Value::Object(Some(mirror_obj)))) =
            crate::classloader_real::load_class_named_as_the_vm(ctx, loader, &dotted)
        {
            if let Some(id) = ctx.class_id_from_mirror(mirror_obj) {
                return Some(id);
            }
        }
    }
    // No Java-level loader object (bootstrap-loaded `this`) or the
    // invocation failed/threw — global lookup is the best available
    // fallback, and is safe here since a null loader means isolation
    // cannot be in play to begin with.
    if let Some(id) = ctx.class_id_by_name(name) {
        return Some(id);
    }
    // DERENCODABLE-SEALED-20260801: `class_id_by_name` is a PASSIVE cache
    // lookup that never triggers classloading, so the branch above only ever
    // answers for a nestmate/permitted subclass that some earlier code
    // happened to load already. That is precisely the hole the
    // WEBCLIENTEXT-SEALED-20260717 fix closed for app classes — but it closed
    // it only inside the `loader_obj` arm above, and a class defined by the
    // BOOTSTRAP loader has no Java-level `ClassLoader` object at all
    // (`native_class_get_class_loader` returns null for every `java/`,
    // `javax/`, `jdk/`, `sun/`, `com/sun/` name), so every JDK-owned sealed
    // interface fell straight through to the passive lookup and reported
    // whichever subset of its permitted subclasses was incidentally loaded.
    //
    // Live measurement on JDK 25's `java.security.DEREncodable` (sealed, 8
    // permitted subclasses, the PEM-encoding JEP): `getPermittedSubclasses0()`
    // returned `[null, null, null, null, null, X509Certificate, null, null]`
    // — one entry, purely because `X509Certificate` was already loaded — where
    // real HotSpot returns all 8. Re-running the same probe *after* touching
    // all 8 classes returned all 8, which is the passive-lookup signature.
    //
    // `load_class` is the same active resolution the loader arm gets from
    // `ClassLoader.loadClass`, and it is equally re-entrancy-safe here (an
    // ordinary native-method call boundary, exactly like `Class.forName`'s
    // native). Guard it with `would_fabricate_synthetic_stub` so a name that
    // has no real class file behind it keeps answering `None` instead of
    // minting a stub and handing back a `Class` that HotSpot would never
    // produce.
    if ctx.would_fabricate_synthetic_stub(name) {
        return None;
    }
    match ctx.load_class(name) {
        Ok(Some(Value::Object(Some(mirror)))) => ctx.class_id_from_mirror(mirror),
        _ => None,
    }
}

pub fn native_class_get_nest_members(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            // The array allocation is a GC point: `this` is re-read through
            // its pin before the store (wave 100, lane L3).
            let this_pin = ctx.pin_native_root(this);
            let class_comp = class_component_id(ctx);
            let arr = ctx.new_ref_array(class_comp, 1);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            ctx.set_array_element(arr, 0, Value::Object(Some(this)));
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    // `this` class's own defining loader is authoritative for resolving
    // its nestmates (compiled together, always visible to that loader) —
    // see `resolve_nestmate_via_defining_loader`'s doc comment for why
    // this must be tried BEFORE any loader-blind global lookup, not after.
    let mut loader_obj: Option<ObjectRef> = None;
    let mut loader_resolved = false;

    // gen r4w3/rooting: the host resolution may run `loadClass` (GC point)
    // and `this` is stored into the `[self]` answer below; pin and re-read.
    let this_pin = ctx.pin_native_root(this);
    // A lambda proxy is a hidden NESTMATE of its defining class: HotSpot's
    // `JVM_GetNestMembers` answers that host's nest (`[host, members...]`),
    // as `getNestHost0` above already answers the host. It answered `[proxy]`
    // (interpreter round i5 wave 100, lane L3; `L3W100LambdaNestMembers`).
    let lambda_host = if is_lambda_proxy_id(class_id) {
        lambda_proxy_nest_host(ctx, class_id)
    } else {
        None
    };
    let host_class_id = match ctx.nest_host_name(class_id) {
        _ if lambda_host.is_some() => lambda_host.unwrap_or(class_id),
        Some(host_name) => resolve_nestmate_via_defining_loader(
            ctx,
            args,
            &host_name,
            &mut loader_obj,
            &mut loader_resolved,
        )
        // JIT round 18 wave 9, lane mhloader: an invalid host makes the
        // class its own nest host (`CRATONVM_CLASS_NEST_HOST_VALIDATE`).
        .filter(|&h| nest_host_is_valid(ctx, class_id, h))
        .unwrap_or(class_id),
        None => class_id,
    };

    let members = ctx.nest_member_names(host_class_id);
    // `Class[]`, the declared type (i6-L2).
    let class_comp = class_component_id(ctx);
    if members.is_empty() {
        // Not a nest host вЂ” return [self], or a lambda proxy's [host]
        // (HotSpot puts the nest host first, whoever asks).
        //
        // `this` stays pinned across the array allocation (a GC point) and
        // the host mirror's materialisation, and is re-read before the
        // store; the array is pinned across the mirror (wave 100, lane L3).
        let arr = ctx.new_ref_array(class_comp, 1);
        let arr_pin = ctx.pin_native_root(arr);
        let first = if host_class_id == class_id {
            ctx.read_native_pin(this_pin, this)
        } else {
            ctx.get_class_mirror(host_class_id)
        };
        let arr = ctx.read_native_pin(arr_pin, arr);
        // Releases `this_pin` and `arr_pin` (taken after it).
        ctx.unpin_native_roots(this_pin);
        ctx.set_array_element(arr, 0, Value::Object(Some(first)));
        Ok(Some(Value::Object(Some(arr))))
    } else {
        ctx.unpin_native_roots(this_pin);
        // Nest host вЂ” return [host] + resolved members.
        //
        // DERENCODABLE-SEALED-20260801: accumulate `ClassId`s, not
        // `ObjectRef`s — see the matching comment in
        // `native_class_get_permitted_subclasses`. Resolution now runs an
        // active `load_class` for bootstrap-loaded nest hosts, so the loop
        // body can allocate and collect; a `Vec<ObjectRef>` carried across it
        // is the Family-1 stale-native-local shape.
        let mut member_ids: Vec<ClassId> = vec![host_class_id];
        for member_name in &members {
            let resolved = resolve_nestmate_via_defining_loader(
                ctx,
                args,
                member_name,
                &mut loader_obj,
                &mut loader_resolved,
            );
            if let Some(member_id) = resolved {
                // HotSpot `JVM_GetNestMembers` keeps a member only when its
                // own (validated) nest host is this host (round 18 wave 9,
                // lane mhloader).
                if member_id != host_class_id && nest_member_is_valid(ctx, member_id, host_class_id)
                {
                    member_ids.push(member_id);
                }
            }
        }
        let arr = ctx.new_ref_array(class_comp, member_ids.len());
        let arr_pin = ctx.pin_native_root(arr);
        for (i, member_id) in member_ids.iter().enumerate() {
            let mirror = ctx.get_class_mirror(*member_id);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
        }
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.unpin_native_roots(arr_pin);
        Ok(Some(Value::Object(Some(arr))))
    }
}

/// A new `byte[]` holding `bytes`, pinned: `(array, pin)`; `None` for no
/// bytes or a failed write. Interpreter round i2 wave 58, lane L3.
pub fn pinned_byte_array(
    ctx: &mut dyn NativeContext,
    bytes: Option<Vec<u8>>,
) -> Option<(ObjectRef, usize)> {
    let bytes = bytes?;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    if !ctx.write_byte_array_from(arr, 0, &bytes) {
        return None;
    }
    Some((arr, ctx.pin_native_root(arr)))
}

/// `java/lang/Class.getRecordComponents0()[Ljava/lang/reflect/RecordComponent;`
///
/// WP2.1: Returns the record components of a record class.
///   * Non-record classes: returns null (per JDK 25 spec).
///   * Record classes: returns array of RecordComponent objects.
///
/// The fields populated on each RecordComponent are:
///   * clazz   вЂ” the declaring record Class mirror
///   * name    вЂ” the component name (String)
///   * type    вЂ” the component type (Class mirror, resolved from descriptor)
///   * accessor вЂ” null (computed lazily by the Java side via getDeclaredMethod)
///   * signature, annotations, typeAnnotations вЂ” null/zero-length
///
/// Fields are populated via `set_field_by_name` so the impl works with both
/// the real-JDK layout (where Class.getName etc. is pure-Java reading
/// `this.name`) and the synthetic mode (where natives at fixed slots are
/// populated via `register_p60_record`).
pub fn native_class_get_record_components(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    if !ctx.is_record_class(class_id) {
        // Non-record returns null (matches JDK Class.getRecordComponents0 contract).
        return Ok(Some(Value::Object(None)));
    }

    let components = ctx.record_components(class_id);
    // gen r4w3/rooting: `RecordComponent`'s <clinit>, `descriptor_to_class_mirror`
    // (user-loader load) and `create_method_object` are GC points; `this`, the
    // result array and each component's pieces are pinned and re-read before
    // every store.
    let this_pin = ctx.pin_native_root(this);
    let rc_class_id = ctx
        .ensure_class_initialized("java/lang/reflect/RecordComponent")
        .unwrap_or(cratonvm_types::ClassId::new(0));

    // Allocate with the JDK-layout total field count, falling back to a
    // 3-slot floor for synthetic mode where the class is unknown.
    let jdk_layout_fields = ctx.class_num_total_fields(rc_class_id);
    let num_fields = if jdk_layout_fields >= 3 {
        jdk_layout_fields
    } else {
        3
    };

    // Resolve declared methods once so we can build the per-component
    // accessor link without re-querying the class manager per component.
    // For `record Foo(int a, String b)`, the Record attribute lists
    // (a, I) and (b, Ljava/lang/String;); the synthesized accessors are
    // `int a()` (descriptor `()I`) and `String b()` (descriptor
    // `()Ljava/lang/String;`) вЂ” i.e. zero-arg, return type matches the
    // component descriptor.
    let declared = ctx.declared_methods(class_id);

    // `RecordComponent[]`, the declared type (i6-L2).
    let arr = ctx.new_ref_array(rc_class_id, components.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, (name, descriptor)) in components.iter().enumerate() {
        // Interpreter round i4 wave 84, lane L3: HotSpot resolves the
        // component's type through the RECORD's loader
        // (`java_lang_reflect_RecordComponent::create` ->
        // `find_java_mirror_for_type(type, holder, NCDFError)`), so a type that
        // loader cannot load is `getRecordComponents()`'s `NoClassDefFoundError`.
        // CratonVM resolved it loader-blind and minted a stand-in mirror for a
        // missing type (`L3W84RecordComponentTypes`). Same switch and scope as
        // the outer-class lookups (`outer_resolution_errors_apply`); a class
        // type resolved this way is the type's mirror.
        let mut resolved_type: Option<ClassId> = None;
        if let Some(inner) = descriptor
            .trim_start_matches('[')
            .strip_prefix('L')
            .and_then(|s| s.strip_suffix(';'))
        {
            if outer_resolution_errors_apply(ctx, class_id) {
                // A descriptor type, not a constant-pool entry: resolved per
                // call, never recorded (wave 21, item 6b).
                match class_seen_by_referencing_class_unrecorded(ctx, class_id, inner) {
                    Ok(id) if !descriptor.starts_with('[') => resolved_type = Some(id),
                    Ok(_) => {}
                    Err(e) => {
                        ctx.unpin_native_roots(this_pin);
                        return Err(e);
                    }
                }
            }
        }
        let rc_obj = ctx.alloc_object(rc_class_id, num_fields);
        let rc_pin = ctx.pin_native_root(rc_obj);
        let name_str = ctx.intern_string(name);
        let name_pin = ctx.pin_native_root(name_str);
        let type_mirror = match resolved_type {
            Some(id) => ctx.get_class_mirror(id),
            None => descriptor_to_class_mirror_via_loader(ctx, descriptor, class_id),
        };
        let type_pin = ctx.pin_native_root(type_mirror);
        // SBR2-CLASSREFL: HotSpot fills `RecordComponent.signature` from the
        // component's Signature attribute, which javac emits identically on the
        // synthesized `private final` field of the same name. The real
        // `getGenericSignature`/`getGenericType` read it.
        //
        // Interpreter round i5 wave 101, lane L3
        // (`L3W101RecordComponentSignature`): HotSpot's
        // `java_lang_reflect_RecordComponent::create` takes the COMPONENT's
        // own Signature attribute and never consults the field; a generated
        // class whose two differ answered the field's.
        let sig_pin = ctx.record_component_signature(class_id, i).map(|s| {
            let o = ctx.intern_string(&s);
            (ctx.pin_native_root(o), o)
        });
        // Interpreter round i2 wave 58, lane L3: the component's
        // `RuntimeVisibleAnnotations` / `RuntimeVisibleTypeAnnotations` bodies,
        // which HotSpot hands the JDK's `AnnotationParser` /
        // `TypeAnnotationParser` in either mode (`L3W57RecordComponentAnnotations`).
        let (ann_bytes, type_ann_bytes) = ctx.record_component_annotation_bytes(class_id, i);
        let ann_pin = pinned_byte_array(ctx, ann_bytes);
        let type_ann_pin = pinned_byte_array(ctx, type_ann_bytes);

        // WP2.1 FAIL-11 fix: build the accessor Method link.
        // JVMS 4.7.30 / JLS 8.10.3: a record component named `n` of type
        // `T` is paired with an accessor method named `n` taking no args
        // and returning `T`.  Match against `declared_methods` by name +
        // exact descriptor `()<component_descriptor>` so we don't pick up
        // a same-named overload that the user happens to have declared.
        let expected_desc = format!("(){}", descriptor);
        let accessor_res = declared
            .iter()
            .find(|m| m.name == *name && m.descriptor == expected_desc)
            .map(|m| create_method_object(ctx, m))
            .transpose();
        let accessor_obj = match accessor_res {
            Ok(obj) => obj,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let accessor_value = match accessor_obj {
            Some(obj) => Value::Object(Some(obj)),
            None => Value::Object(None),
        };
        // gen r4w3/rooting: current addresses after the GC points above.
        let this = ctx.read_native_pin(this_pin, this);
        let arr = ctx.read_native_pin(arr_pin, arr);
        let rc_obj = ctx.read_native_pin(rc_pin, rc_obj);
        let name_str = ctx.read_native_pin(name_pin, name_str);
        let type_mirror = ctx.read_native_pin(type_pin, type_mirror);
        let sig_value = match sig_pin {
            Some((pin, o)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            None => Value::Object(None),
        };
        let ann_value = Value::Object(ann_pin.map(|(a, pin)| ctx.read_native_pin(pin, a)));
        let type_ann_value =
            Value::Object(type_ann_pin.map(|(a, pin)| ctx.read_native_pin(pin, a)));
        ctx.unpin_native_roots(rc_pin);

        // Populate by-name (works for both real-JDK layout and synthetic).
        ctx.set_field_by_name(rc_obj, "clazz", Value::Object(Some(this)));
        ctx.set_field_by_name(rc_obj, "name", Value::Object(Some(name_str)));
        ctx.set_field_by_name(rc_obj, "type", Value::Object(Some(type_mirror)));
        ctx.set_field_by_name(rc_obj, "accessor", accessor_value);
        ctx.set_field_by_name(rc_obj, "signature", sig_value);
        ctx.set_field_by_name(rc_obj, "annotations", ann_value);
        ctx.set_field_by_name(rc_obj, "typeAnnotations", type_ann_value);

        // Belt-and-suspenders: also populate the synthetic fixed-slot layout
        // (slot 0=name, slot 1=type, slot 2=declaringRecord) used by
        // `lang_misc::register_p60_record`. Real-JDK layout reads via getfield
        // so this duplication is harmless when the class has its own field
        // mapping that overrides.
        if jdk_layout_fields == 0 {
            ctx.set_field(rc_obj, 0, Value::Object(Some(name_str)));
            ctx.set_field(rc_obj, 1, Value::Object(Some(type_mirror)));
            ctx.set_field(rc_obj, 2, Value::Object(Some(this)));
        }

        ctx.set_array_element(arr, i, Value::Object(Some(rc_obj)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(arr))))
}

/// `java/lang/Class.getPermittedSubclasses0()[Ljava/lang/Class;`
///
/// Returns the permitted subclasses of a sealed class, or null if not sealed.
///
/// WEBCLIENTEXT-SEALED-20260717: each permitted-subclass NAME comes straight
/// from the classfile's `PermittedSubclasses` attribute (always correct --
/// see `is_sealed_class`/`permitted_subclasses`, both backed by the same
/// parsed field), but resolving those names to `ClassId`s used to go through
/// `ctx.class_id_by_name`, a PASSIVE cache lookup that never triggers
/// classloading. A permitted subclass that simply hasn't been loaded yet
/// (a very common ordering -- reflective code, e.g. kotlin-reflect's
/// `KClass.sealedSubclasses` behind mockk's `ProxyMaker`, routinely probes a
/// sealed interface's metadata before the app ever touches its concrete
/// subclasses) silently produced a `null` array slot instead of the resolved
/// `Class`. Real JDK's own `Class.getPermittedSubclasses()` Java wrapper then
/// filters out any entry that doesn't verifiably extend/implement `this`
/// (`c.getSuperclass() == this || asList(c.getInterfaces()).contains(this)`)
/// -- a `null` element fails that check, so the whole array silently
/// collapsed to empty (confirmed via a live probe: `getPermittedSubclasses0()`
/// returned `[null, null]` for `org.springframework.http.HttpStatusCode`,
/// and the public wrapper reduced that to `[]`), which is exactly the
/// `IllegalStateException: Unable to create proxy for sealed class ...,
/// no subclasses available` mockk throws when `sealedSubclasses` comes back
/// empty. Same root-cause family as the `getEnclosingClass`/
/// `getDeclaringClass` loader-identity fix (`aca7f635`): resolve ACTIVELY,
/// not passively. Mirrors `declaring_class_loader_aware`'s re-entrant
/// `loadClass()` pattern -- safe here because this is an ordinary native
/// method call boundary (like `Class.forName`'s native, which already does
/// the same re-entrant `loadClass()` call routinely), not the interpreter's
/// raw opcode-dispatch fast path (where an earlier, unrelated attempt at a
/// similar re-entrant fix for `getstatic`/`putstatic` caused heap corruption
/// and was reverted -- see docs/known-issues, ImportHttpServiceRegistrarTests
/// entry). Falls back to the passive cache lookup first (cheap, handles the
/// overwhelming common case where the subclass is already loaded) and only
/// pays for the active `loadClass()` round-trip on a genuine cache miss.
pub fn native_class_get_permitted_subclasses(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    if !ctx.is_sealed_class(class_id) {
        return Ok(Some(Value::Object(None)));
    }

    let subs = ctx.permitted_subclasses(class_id);
    // The same ClassLoader that defines `this` sealed class is authoritative
    // for resolving its own permitted subclasses (they are compiled
    // together, always visible to that loader) — see
    // `resolve_nestmate_via_defining_loader`'s doc comment for why this must
    // be tried BEFORE any loader-blind global lookup, not after.
    let mut loader_obj: Option<ObjectRef> = None;
    let mut loader_resolved = false;
    // DERENCODABLE-SEALED-20260801: COMPACT, never leave holes. The array this
    // native returns is handed straight to real JDK bytecode
    // (`Class.getPermittedSubclasses()` → `isDirectSubType(c)` →
    // `c.getInterfaces(false)`), which dereferences every element without a
    // null guard because HotSpot's own `getPermittedSubclasses0` cannot
    // produce a null element. A hole here therefore becomes a null-receiver
    // instance call inside `java.lang.Class` — and the two ways CratonVM
    // answers that call (a null-tolerant slow-path shim that returns an empty
    // `Class[]`, vs. a warmed inline cache that pushes the callee frame with
    // `this == null`) produce, respectively, a silently EMPTY permitted set
    // and `NullPointerException: Cannot read field "interfaces" because "rd"
    // is null` at `Class.java:1217`. Both were observed on the same JDK 25
    // `DEREncodable` hierarchy — the second one is what aborts every Mockito
    // `mock(X509Certificate.class)`.
    //
    // The active resolution above should now leave no holes at all; compacting
    // is the belt to that braces, and it keeps the JDK contract ("every
    // element is a real `Class`") true by construction for any future name
    // that genuinely cannot be resolved.
    //
    // Accumulate `ClassId`s, not `ObjectRef`s: resolution now runs
    // `ClassLoader.loadClass` / `load_class` per name, either of which can
    // allocate and therefore collect. A `Vec<ObjectRef>` held across that loop
    // is the Family-1 stale-native-local shape; a `ClassId` is a plain index
    // the GC never rewrites. Mirrors are materialised afterwards, one at a
    // time, with the destination array pinned (`get_class_mirror` allocates on
    // first use for a class whose mirror the active resolution above just
    // brought in — precisely the new case this fix creates).
    let mut sub_ids: Vec<ClassId> = Vec::with_capacity(subs.len());
    for sub_name in subs.iter() {
        let resolved = resolve_nestmate_via_defining_loader(
            ctx,
            args,
            sub_name,
            &mut loader_obj,
            &mut loader_resolved,
        );
        if let Some(sub_id) = resolved {
            sub_ids.push(sub_id);
        }
    }
    // `Class[]`, the declared type (i6-L2).
    let class_comp = class_component_id(ctx);
    let arr = ctx.new_ref_array(class_comp, sub_ids.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, sub_id) in sub_ids.iter().enumerate() {
        let mirror = ctx.get_class_mirror(*sub_id);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(arr))))
}

/// JVMS 4.7.20's `CLASS_EXTENDS` target's `supertype_index` value that
/// selects the superclass (as opposed to `0..n` selecting the n-th entry of
/// `getInterfaces()`).
pub const CLASS_EXTENDS_SUPERCLASS_INDEX: u16 = 0xFFFF;

/// `java/lang/Class.getAnnotatedSuperclass()Ljava/lang/reflect/AnnotatedType;`
///
/// WP2.1-class-modern: returns an `AnnotatedType` for the direct superclass
/// of this Class, wrapping the erased superclass `Class` mirror and carrying
/// any TYPE_USE annotations placed directly on the superclass itself (e.g.
/// `extends @Foo Bar`) via [`NativeContext::class_extends_type_annotations`]
/// with `supertype_index == 0xFFFF`. Returns null for `Object`, primitive
/// types, void, array types, and interfaces вЂ” matching the JDK contract.
///
/// Frameworks that probe `Class.getAnnotatedSuperclass()` usually only
/// need it to be non-null + not throw (Hibernate's
/// `ReflectionUtil.scanForAnnotatedTypes`); the annotated backing is
/// sufficient to keep their `<clinit>` chain alive.
pub fn native_class_get_annotated_superclass(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    // Match JDK contract: null for Object, primitives, void, array, interfaces.
    let name = ctx.class_name_of_id(class_id).unwrap_or_default();
    if name == "java/lang/Object" || name.starts_with('[') || name.is_empty() {
        return Ok(Some(Value::Object(None)));
    }

    // Resolve the direct superclass via `NativeContext::superclass_of`.
    let super_mirror = match ctx.superclass_of(class_id) {
        Some(sid) => ctx.get_class_mirror(sid),
        None => return Ok(Some(Value::Object(None))),
    };

    let tree = ctx.class_extends_type_annotations(class_id, CLASS_EXTENDS_SUPERCLASS_INDEX);
    let at = make_annotated_type_with_anns(ctx, super_mirror, &tree.anns, Some(class_id))?;
    if !tree.children.is_empty() {
        stash_annotated_type_argument_anns(&*ctx, at, tree.children);
    }
    Ok(Some(Value::Object(Some(at))))
}

/// `java/lang/Class.getAnnotatedInterfaces()[Ljava/lang/reflect/AnnotatedType;`
///
/// WP2.1-class-modern: returns an `AnnotatedType[]` mirroring the
/// `getGenericInterfaces()` array (residual-2 fix вЂ” see below for why the
/// erased `getInterfaces()` array is wrong here). Each element is an
/// `AnnotatedType` wrapping the corresponding interface Type, carrying that
/// interface's TYPE_USE annotations (both directly on the interface itself,
/// e.g. `implements @Foo Bar`, and -- via the recursive stash consumed by
/// [`native_annotated_parameterized_type_get_annotated_actual_type_arguments`]
/// -- nested in its type arguments to any depth, e.g. `implements
/// ValueExtractor<ArgumentValue<@ExtractedValue ?>>`) from
/// [`NativeContext::class_extends_type_annotations`].
///
/// HV000203 fix: Hibernate Validator's `ValueExtractorResolver` registers a
/// custom `jakarta.validation.valueextraction.ValueExtractor<T>` by walking
/// `getAnnotatedInterfaces()` to find which type parameter is annotated
/// `@ExtractedValue`. Every element used to come back via [`make_annotated_type`]
/// with an unconditionally EMPTY `Annotation[]` -- so `@ExtractedValue` was
/// unreachable regardless of nesting depth, and Validator rejected any
/// extractor whose annotation sits on a nested type argument (Spring
/// GraphQL's `ArgumentValueValueExtractor` implements
/// `ValueExtractor<ArgumentValue<@ExtractedValue ?>>`, two levels deep) with
/// `HV000203: ... fails to declare the extracted type parameter using
/// @ExtractedValue`, breaking `ControllerEndpointDiscovererTests` and the
/// `spring-boot-graphql-test` module's `defaultValidator` bean at startup.
///
/// Always returns a non-null (possibly zero-length) array вЂ” matching the
/// JDK contract. Frameworks (ByteBuddy, JMX OpenMBean introspector) rely
/// on the non-null guarantee; throwing or returning null here breaks
/// `MBeanIntrospector.getMethods` recursion.
///
/// residual-2 fix: this must wrap the GENERIC interface types (from
/// `getGenericInterfaces()`, which parses the class's Signature attribute
/// into real `ParameterizedTypeImpl`s), not the erased `Class` mirrors from
/// `getInterfaces()`. An erased `Class` always looks like a plain type to
/// `annotated_type_impl_class_name`, so a generic interface such as
/// `Comparable<Path>`/`Iterable<Path>` (two of `java.nio.file.Path`'s direct
/// superinterfaces) got wrapped in the base `AnnotatedTypeBaseImpl` instead
/// of `AnnotatedParameterizedTypeImpl` вЂ” the same ByteBuddy `AnnotatedType`
/// dispatch mismatch the ES bytebuddy-annotatedtype fix addressed for
/// methods/fields/parameters (see
/// `elasticsearch-bytebuddy-annotatedtype-proxy-mismatch.md`),
/// just not yet applied here. Mockito's `mock(Path.class)` walks exactly
/// this path via ByteBuddy's owner-type reader when generating the mock.
pub fn native_class_get_annotated_interfaces(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let class_id = obj_arg(args, 0)
        .ok()
        .and_then(|this| mirror_class_id(ctx, this));
    let generic_ifaces = match native_class_get_generic_interfaces(ctx, args)? {
        Some(Value::Object(Some(arr))) => arr,
        _ => {
            let arr = empty_reflection_array(ctx, "java/lang/reflect/AnnotatedType");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let len = ctx.array_length(generic_ifaces);
    // `AnnotatedType[]`, the declared type (i6-L2).
    let annotated_type = reflection_component_id(ctx, "java/lang/reflect/AnnotatedType");
    let arr = ctx.new_ref_array(annotated_type, len);
    // gen r4w3/rooting: `make_annotated_type_with_anns` allocates/initialises
    // (GC point) once per element; pin the source and result arrays and
    // re-read both every iteration.
    let src_pin = ctx.pin_native_root(generic_ifaces);
    let arr_pin = ctx.pin_native_root(arr);
    for i in 0..len {
        let generic_ifaces = ctx.read_native_pin(src_pin, generic_ifaces);
        if let Value::Object(Some(iface_type)) = ctx.get_array_element(generic_ifaces, i) {
            let tree = class_id
                .map(|cid| ctx.class_extends_type_annotations(cid, i as u16))
                .unwrap_or_default();
            let at = match make_annotated_type_with_anns(ctx, iface_type, &tree.anns, class_id) {
                Ok(at) => at,
                Err(e) => {
                    ctx.unpin_native_roots(src_pin);
                    return Err(e);
                }
            };
            if !tree.children.is_empty() {
                stash_annotated_type_argument_anns(&*ctx, at, tree.children);
            }
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(at)));
        }
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(src_pin);
    Ok(Some(Value::Object(Some(arr))))
}

/// `TypeVariable.getAnnotatedBounds()[Ljava/lang/reflect/AnnotatedType;`.
///
/// Synthetic TypeVariable mirrors have a native `getBounds()` but the JDK's
/// default `getAnnotatedBounds()` body cannot run on the synthetic interface
/// receiver. Materialize the same bound list as minimal AnnotatedTypes so
/// ByteBuddy can inspect a mocked type's generic methods.
pub fn native_type_variable_get_annotated_bounds(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bounds = match ctx.invoke_virtual(this, "getBounds", "()[Ljava/lang/reflect/Type;", &[])? {
        Some(Value::Object(Some(bounds))) => bounds,
        _ => {
            let empty = empty_reflection_array(ctx, "java/lang/reflect/AnnotatedType");
            return Ok(Some(Value::Object(Some(empty))));
        }
    };

    let len = ctx.array_length(bounds);
    // `AnnotatedType[]`, the declared type (i6-L2).
    let annotated_type = reflection_component_id(ctx, "java/lang/reflect/AnnotatedType");
    let annotated = ctx.new_ref_array(annotated_type, len);
    // `make_annotated_type` initialises and allocates (a GC point) once per
    // bound; pin the source and result arrays and re-read both every turn,
    // as `native_class_get_annotated_interfaces` does.
    let bounds_pin = ctx.pin_native_root(bounds);
    let annotated_pin = ctx.pin_native_root(annotated);
    for i in 0..len {
        let bounds = ctx.read_native_pin(bounds_pin, bounds);
        if let Value::Object(Some(bound)) = ctx.get_array_element(bounds, i) {
            let annotated_bound = make_annotated_type(ctx, bound);
            let annotated = ctx.read_native_pin(annotated_pin, annotated);
            ctx.set_array_element(annotated, i, Value::Object(Some(annotated_bound)));
        }
    }
    let annotated = ctx.read_native_pin(annotated_pin, annotated);
    ctx.unpin_native_roots(bounds_pin);
    Ok(Some(Value::Object(Some(annotated))))
}

/// Build a minimal synthetic `AnnotatedType` object wrapping a `Type`
/// (typically a `Class` mirror).
///
/// Layout (2 fields, by-name + slot-fallback):
///   * `type` вЂ” the wrapped `Type` (slot 0)
///   * `annotations` вЂ” empty `Annotation[]` (slot 1)
///
/// `AnnotatedType` is an interface in the JDK; its concrete impl class is
/// `sun.reflect.annotation.AnnotatedTypeFactory$AnnotatedTypeBaseImpl` /
/// `AnnotatedTypeImpl`. We allocate against `java/lang/reflect/AnnotatedType`
/// вЂ” the dispatch path treats this as a synthetic-stub instance.
/// `getType()` reads slot 0 ; downstream frameworks only need that
/// accessor + non-null-ness.
/// Fill the real-JDK `AnnotatedTypeBaseImpl` bookkeeping fields that the
/// bytecode paths we *don't* override still read. Specifically `location` must
/// be non-null: `AnnotatedTypeBaseImpl.getAnnotatedOwnerType()` (not overridden)
/// does `getLocation().popLocation((byte)1)` and NPEs on a null `location`.
/// ByteBuddy walks owner types of parameterized method types (mock generation),
/// hitting exactly this path вЂ” surfaced as
/// `MockitoException вЂ¦ NullPointerException вЂ¦ AnnotatedTypeFactory.getLocation()`
/// (`UUidV6V7GeneratorTest`). Seed `location` with `LocationInfo.BASE_LOCATION`
/// (depth 0 в†’ `popLocation` returns null в†’ the owner is rebuilt with
/// BASE_LOCATION by real-JDK code) and `allOnSameTargetTypeAnnotations` with an
/// empty array so `getTypeAnnotations()` never returns null either.
pub fn annotated_type_fill_bookkeeping(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    // GC-SAFETY: `ensure_class_initialized` (via `base_loc`'s computation)
    // and `new_ref_array` below can each trigger a moving GC; `obj` (the
    // caller-supplied AnnotatedType instance) is used both before and
    // after both. Pin up front and re-read before each use.
    let obj_pin = ctx.pin_native_root(obj);
    let base_loc = ctx
        .ensure_class_initialized("sun/reflect/annotation/TypeAnnotation$LocationInfo")
        .ok()
        .and_then(|cid| {
            ctx.static_field_index_by_name(cid, "BASE_LOCATION")
                .map(|idx| ctx.get_static_field(cid, idx))
        })
        .unwrap_or(Value::Object(None));
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field_by_name(obj, "location", base_loc);
    // `TypeAnnotation[]`, the field's declared type (i6-L2).
    let empty_ta = empty_reflection_array(ctx, "sun/reflect/annotation/TypeAnnotation");
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field_by_name(
        obj,
        "allOnSameTargetTypeAnnotations",
        Value::Object(Some(empty_ta)),
    );
    ctx.unpin_native_roots(obj_pin);
}

/// Which real-JDK `sun.reflect.annotation.AnnotatedTypeFactory` impl class
/// should back an `AnnotatedType` wrapping `backing_type`, mirroring
/// `AnnotatedTypeFactory.buildAnnotatedType`'s own dispatch: an array `Type`
/// (array `Class` or `GenericArrayType`) в†’ `AnnotatedArrayTypeImpl`,
/// `TypeVariable` в†’ `AnnotatedTypeVariableImpl`, `ParameterizedType` в†’
/// `AnnotatedParameterizedTypeImpl`, `WildcardType` в†’ `AnnotatedWildcardTypeImpl`,
/// else (plain `Class`) в†’ the base impl. All four subclasses extend
/// `AnnotatedTypeBaseImpl` and declare no extra fields, so the base class's
/// 4-field layout (`type`, `location`, `allOnSameTargetTypeAnnotations`,
/// `annotations`) is inherited unchanged вЂ” picking the right subclass only
/// changes which `AnnotatedXxxType` sub-interface the constructed object
/// satisfies via `instanceof`.
///
/// Always building the base class regardless of `Type` kind is exactly the
/// gap that broke ByteBuddy's `JavaDispatcher` (used by Mockito): its own
/// reflective readers assume real JDK's per-kind subclasses (e.g. checking
/// `instanceof AnnotatedParameterizedType` / calling
/// `getAnnotatedActualTypeArguments()`, which only that subclass declares)
/// and fail with a receiver-type mismatch a few frames up the call chain
/// when handed a plain `AnnotatedTypeBaseImpl` instead вЂ” see
/// `elasticsearch-bytebuddy-annotatedtype-proxy-mismatch.md`.
pub fn annotated_type_impl_class_name(
    ctx: &mut dyn NativeContext,
    backing_type: cratonvm_types::ObjectRef,
) -> &'static str {
    const BASE: &str = "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl";
    const ARRAY: &str = "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedArrayTypeImpl";
    const TYPE_VAR: &str = "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeVariableImpl";
    const PARAMETERIZED: &str =
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedParameterizedTypeImpl";
    const WILDCARD: &str = "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedWildcardTypeImpl";

    let cid = ctx.class_id_of_object(backing_type);
    match ctx.class_name_of_id(cid).unwrap_or_default().as_str() {
        "java/lang/reflect/GenericArrayType"
        | "sun/reflect/generics/reflectiveObjects/GenericArrayTypeImpl" => ARRAY,
        "java/lang/reflect/TypeVariable"
        | "sun/reflect/generics/reflectiveObjects/TypeVariableImpl" => TYPE_VAR,
        "java/lang/reflect/ParameterizedType"
        | "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl" => PARAMETERIZED,
        "java/lang/reflect/WildcardType"
        | "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl" => WILDCARD,
        "java/lang/Class" => {
            // A Class is only an "array Type" (needing AnnotatedArrayTypeImpl)
            // when it represents an array type; otherwise it's a plain type.
            if mirror_class_name(ctx, backing_type)
                .map(|n| n.starts_with('['))
                .unwrap_or(false)
            {
                ARRAY
            } else {
                BASE
            }
        }
        _ => BASE,
    }
}

pub fn make_annotated_type(
    ctx: &mut dyn NativeContext,
    backing_type: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    // Select the real-JDK impl class matching the backing Type's kind (see
    // `annotated_type_impl_class_name`); fall back to the interface name
    // (synthetic-mode placeholder) if the impl class can't be loaded.
    let impl_name = annotated_type_impl_class_name(ctx, backing_type);
    // GC-SAFETY: `ensure_class_initialized`, `alloc_object`,
    // `new_ref_array`, and (in the `else` branch) `annotated_type_fill_
    // bookkeeping`'s own internal allocations can each trigger a moving
    // GC; `backing_type` (the caller-supplied Type mirror) and `obj` (the
    // freshly-allocated AnnotatedType, also the return value) are each
    // used again after one of these. Pin `backing_type` up front and
    // `obj` as soon as it's allocated, and re-read both before each
    // subsequent use.
    let backing_pin = ctx.pin_native_root(backing_type);
    let cid = ctx
        .ensure_class_initialized(impl_name)
        .or_else(|_| ctx.ensure_class_initialized("java/lang/reflect/AnnotatedType"))
        .unwrap_or(cratonvm_types::ClassId::new(0));

    let layout_fields = ctx.class_num_total_fields(cid);
    let num_fields = if layout_fields >= 2 { layout_fields } else { 2 };
    let obj = ctx.alloc_object(cid, num_fields);
    let obj_pin = ctx.pin_native_root(obj);

    // empty Annotation[] for `annotations`
    let empty_anns = empty_reflection_array(ctx, "java/lang/annotation/Annotation");
    let backing_type = ctx.read_native_pin(backing_pin, backing_type);
    let obj = ctx.read_native_pin(obj_pin, obj);

    ctx.set_field_by_name(obj, "type", Value::Object(Some(backing_type)));
    ctx.set_field_by_name(obj, "annotations", Value::Object(Some(empty_anns)));
    if layout_fields == 0 {
        ctx.set_field(obj, 0, Value::Object(Some(backing_type)));
        ctx.set_field(obj, 1, Value::Object(Some(empty_anns)));
    } else {
        annotated_type_fill_bookkeeping(ctx, obj);
    }
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(backing_pin);
    obj
}

/// Like [`make_annotated_type`] but stashes a populated `Annotation[]` (built
/// from `anns`) in the `annotations` field instead of an empty array. The
/// AnnotatedType accessor overrides (`native_annotated_type_get_*`) read that
/// array, so `getAnnotatedReturnType()` / `Parameter.getAnnotatedType()` surface
/// TYPE_USE annotations (JSpecify `@Nullable` / `@NonNull`) that the real-JDK
/// path can't decode (CratonVM stubs `getTypeAnnotationBytes0` to null and does
/// not expose the `jdk.internal.reflect.ConstantPool` accessor API).
pub fn make_annotated_type_with_anns(
    ctx: &mut dyn NativeContext,
    backing_type: ObjectRef,
    anns: &[cratonvm_native_api::AnnotationData],
    declaring_class_id: Option<ClassId>,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-SAFETY: `build_annotation_array`, `ensure_class_initialized`,
    // `alloc_object`, and (in the `else` branch) `annotated_type_fill_
    // bookkeeping`'s own internal allocations can each trigger a moving
    // GC. `backing_type` (the caller-supplied Type mirror) is used again
    // after several of these, and `ann_arr`/`obj` (also the return value)
    // are each used again after later ones. Pin everything as it's
    // produced and re-read before each subsequent use.
    let backing_pin = ctx.pin_native_root(backing_type);
    // Build the proxy array first (it allocates) before we allocate the
    // AnnotatedType object, mirroring the GC-ordering used elsewhere.
    let ann_arr = build_annotation_array_for(ctx, declaring_class_id, anns)?;
    let ann_pin = ctx.pin_native_root(ann_arr);
    let backing_type = ctx.read_native_pin(backing_pin, backing_type);

    // Select the real-JDK impl class matching the backing Type's kind (see
    // `annotated_type_impl_class_name`); fall back to the interface name
    // (synthetic-mode placeholder) if the impl class can't be loaded.
    let impl_name = annotated_type_impl_class_name(ctx, backing_type);
    let cid = ctx
        .ensure_class_initialized(impl_name)
        .or_else(|_| ctx.ensure_class_initialized("java/lang/reflect/AnnotatedType"))
        .unwrap_or(cratonvm_types::ClassId::new(0));

    let layout_fields = ctx.class_num_total_fields(cid);
    let num_fields = if layout_fields >= 2 { layout_fields } else { 2 };
    let obj = ctx.alloc_object(cid, num_fields);
    let obj_pin = ctx.pin_native_root(obj);
    let backing_type = ctx.read_native_pin(backing_pin, backing_type);
    let ann_arr = ctx.read_native_pin(ann_pin, ann_arr);
    let obj = ctx.read_native_pin(obj_pin, obj);

    ctx.set_field_by_name(obj, "type", Value::Object(Some(backing_type)));
    ctx.set_field_by_name(obj, "annotations", Value::Object(Some(ann_arr)));
    if layout_fields == 0 {
        ctx.set_field(obj, 0, Value::Object(Some(backing_type)));
        ctx.set_field(obj, 1, Value::Object(Some(ann_arr)));
    } else {
        annotated_type_fill_bookkeeping(ctx, obj);
    }
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(backing_pin);
    Ok(obj)
}

/// Side-table holding, per constructed `AnnotatedType` object (keyed by
/// pointer identity -- same pattern/caveats as `constructor_mirror_side_table`:
/// short-lived objects, not expected to survive a moving GC between
/// construction and the `getAnnotatedActualTypeArguments()` read that
/// consumes them), the TYPE_ARGUMENT-level annotation *subtree* for a
/// *parameterized* type, indexed by type-argument position. Each
/// [`TypeArgAnnotations`] node carries both this level's own annotations and
/// (in `.children`) the next nesting level's subtree, so a chain of
/// `getAnnotatedActualTypeArguments()` calls (one per generic nesting level,
/// e.g. `ValueExtractor<ArgumentValue<@ExtractedValue ?>>`) each re-stash
/// their own remaining subtree for the next call to consume -- see
/// [`native_annotated_parameterized_type_get_annotated_actual_type_arguments`].
///
/// Populated by [`native_executable_get_annotated_parameter_types`] (which has
/// access to the owning method + parameter index needed to look up the
/// per-type-argument annotations), by the other `getAnnotatedXxxType`
/// natives (return type / field type / class supertypes), and recursively by
/// [`native_annotated_parameterized_type_get_annotated_actual_type_arguments`]
/// itself. Not every `AnnotatedType` has an entry here -- only ones whose
/// type arguments carry TYPE_USE annotations (e.g. `List<@Valid Person>`);
/// absence means "no per-argument annotations", not an error.
///
/// gen r4w4/rooting2: keyed by `(vm_identity, identity_hash_code)`, not by
/// address. The address key assumed the object would not move between the
/// stash and the read, but the read is whatever later
/// `getAnnotatedActualTypeArguments()` call user code makes, and a moving
/// young cycle in between left the row unreachable (the TYPE_USE
/// annotations silently vanished). Worse, the young generation reuses its
/// from-space addresses every cycle, so a later, unrelated `AnnotatedType`
/// allocated at a dead one's address was handed the dead one's annotations.
/// The identity hash moves with the object and is minted fresh per object,
/// and `vm_identity` keeps two VMs in one process apart, as the other
/// side tables in this file do.
///
/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 16): keyed by the `AnnotatedType`'s WEAK LOCK KEY
/// (`crate::gc_stable_weak_lock_key`), minted by the stash, looked up (never
/// minted) by the read. Two live `AnnotatedType`s with one identity hash (the
/// per-heap counter wraps) shared the `(vm, hash)` row, so B's
/// `getAnnotatedActualTypeArguments()` answered A's TYPE_USE annotations
/// (Bean Validation's `@Valid`). And nothing ever removed a row: one per
/// annotated type ever built, for the life of the VM. Now the lock-key sweep
/// frees a dead `AnnotatedType`'s key and [`forget_lang_class_keys`] drops its
/// row. The value carries the VM for [`forget_vm_lang_class_tables`].
///
/// gc-common w33-a: the 8-byte object header made such collisions routine
/// (a compact instance's hash is 20 counter bits plus 11 class bits, so two
/// `AnnotatedType`s of one class collide every 2^20 hash mints). The weak
/// lock key does not care: the registry tells colliders apart by address.
pub fn annotated_parameterized_type_arg_anns_table(
) -> &'static Mutex<FxHashMap<AnnotatedTypeArgKey, (usize, Vec<TypeArgAnnotations>)>> {
    static TABLE: OnceLock<
        Mutex<FxHashMap<AnnotatedTypeArgKey, (usize, Vec<TypeArgAnnotations>)>>,
    > = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// The weak lock key of an `AnnotatedType`; see
/// [`annotated_parameterized_type_arg_anns_table`].
pub type AnnotatedTypeArgKey = usize;

/// `at_obj` must be CURRENT (every caller stashes right after allocating it).
pub fn stash_annotated_type_argument_anns(
    ctx: &dyn NativeContext,
    at_obj: ObjectRef,
    per_arg_anns: Vec<TypeArgAnnotations>,
) {
    if per_arg_anns
        .iter()
        .all(|v| v.anns.is_empty() && v.children.is_empty())
    {
        return;
    }
    // Minted before the table guard.
    let Ok(key) = crate::gc_stable_weak_lock_key(ctx, at_obj) else {
        return;
    };
    let vm = ctx.vm_identity();
    annotated_parameterized_type_arg_anns_table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, (vm, per_arg_anns));
}

/// Never mints: an `AnnotatedType` nothing was stashed for has no row.
pub fn take_annotated_type_argument_anns(
    ctx: &dyn NativeContext,
    at_obj: ObjectRef,
) -> Option<Vec<TypeArgAnnotations>> {
    let key = crate::existing_weak_lock_key(ctx, at_obj)?;
    annotated_parameterized_type_arg_anns_table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
        .map(|(_, anns)| anns.clone())
}

/// Read the stashed `Annotation[]` from an AnnotatedType built by
/// [`make_annotated_type`] / [`make_annotated_type_with_anns`], returning
/// `(array_ref, len)`. Returns `None` when the `annotations` field is null or
/// not an array.
///
/// A real-JDK `AnnotatedTypeBaseImpl` (built by bytecode paths we don't
/// intercept) stores a `Map` here, not an `Annotation[]` вЂ” `object_is_array`
/// distinguishes the two by heap object kind (a class-name check can't: a
/// heap reference array reports its *component* class, not a `[LвЂ¦;` class).
/// Those real instances carry no top-level type annotations anyway, since
/// `getTypeAnnotationBytes0` is null, so reporting them as empty is correct.
pub fn annotated_type_stashed_anns(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(ObjectRef, usize)> {
    let arr = match ctx.get_field_by_name(this, "annotations") {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    if !ctx.object_is_array(arr) {
        return None;
    }
    Some((arr, ctx.array_length(arr)))
}

/// One rung of a type-mirror resolution LADDER: keep the fallback, keep the
/// exception.
///
/// W7-26. The `AnnotatedType` builders below are written as ladders — try
/// `getGenericReturnType()`, else the erased `getReturnType()`, else a
/// `ClassId(0)` mirror — and every rung was spelled `_ =>`, which catches
/// `Err` along with the "this rung had no answer" cases. That is the same
/// species as the `getAnnotation` sites this record is named for: a thrown
/// exception became an `AnnotatedType` wrapping the WRONG type (usually the
/// `ClassId(0)` mirror), which is a plausible-looking object no caller can
/// tell from a real one.
///
/// The two `MethodCallFailed` variants are not the same thing and the ladder
/// only ever meant one of them:
///
/// * `ExceptionThrown` — a real Java exception is pending in the VM (a
///   malformed `Signature` attribute raising `GenericSignatureFormatError`, a
///   `TypeNotPresentException` from a class-valued member). HotSpot propagates
///   these out of `getAnnotatedReturnType` too, and continuing to call back
///   into the VM with an exception pending is what HotSpot's `CHECK_` macros
///   exist to prevent. Re-raised.
/// * `InternalError` — the receiver has no such method at all, which is
///   exactly the "unavailable, use the next rung" case the ladders were
///   written for (a CratonVM-built `Method`, a synthetic-JDK receiver). Mapped
///   to `Ok(None)` so the existing fallback runs unchanged.
///
/// So no non-throwing path moves in either mode; only a previously-vanishing
/// exception now reaches its caller.
pub fn ladder_rung(result: MethodCallResult) -> MethodCallResult {
    match result {
        Ok(value) => Ok(value),
        Err(e @ MethodCallFailed::ExceptionThrown(_)) => Err(e),
        Err(MethodCallFailed::InternalError(_)) => Ok(None),
    }
}

/// `Method.getAnnotatedReturnType()Ljava/lang/reflect/AnnotatedType;`
///
/// Builds an AnnotatedType wrapping the return type and carrying the
/// method's METHOD_RETURN TYPE_USE annotations. Uses the GENERIC return type
/// (`getGenericReturnType()`, falling back to the erased `Class` only when
/// that call yields nothing) вЂ” not the erased one вЂ” so a parameterized/
/// array/type-variable/wildcard return type reifies as a real
/// `ParameterizedType`/`GenericArrayType`/etc. and `make_annotated_type_with_anns`
/// can pick the matching `AnnotatedXxxType` impl class (see
/// `annotated_type_impl_class_name`). Building the AnnotatedType from the
/// erased type reflects a plain `Class` even for a generic return type,
/// which real JDK never does вЂ” third-party reflective readers that check
/// `instanceof AnnotatedParameterizedType` (ByteBuddy's `JavaDispatcher`,
/// used by Mockito) then diverge from HotSpot.
pub fn native_method_get_annotated_return_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (declaring_class_id, anns, type_arg_anns) = match method_class_name_desc(ctx, this) {
        Some((cid, name, desc)) => (
            Some(cid),
            ctx.method_return_type_annotations(cid, &name, &desc),
            ctx.method_return_type_argument_annotations(cid, &name, &desc),
        ),
        None => (None, Vec::new(), Vec::new()),
    };
    // W7-26 — `ladder_rung` keeps both fallbacks and re-raises a real pending
    // exception instead of reflecting the `ClassId(0)` mirror as this method's
    // return type.
    // gen r4w3/rooting: the first upcall can collect, and `this` is the
    // receiver of the fallback upcall; pin it and re-read before reuse.
    let this_pin = ctx.pin_native_root(this);
    let type_mirror = match ladder_rung(ctx.invoke_virtual(
        this,
        "getGenericReturnType",
        "()Ljava/lang/reflect/Type;",
        &[],
    )) {
        Ok(Some(Value::Object(Some(m)))) => Ok(m),
        Ok(_) => {
            let this = ctx.read_native_pin(this_pin, this);
            match ladder_rung(ctx.invoke_virtual(this, "getReturnType", "()Ljava/lang/Class;", &[]))
            {
                Ok(Some(Value::Object(Some(m)))) => Ok(m),
                Ok(_) => Ok(ctx.get_class_mirror(cratonvm_types::ClassId::new(0))),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    };
    ctx.unpin_native_roots(this_pin);
    let type_mirror = type_mirror?;
    let at = make_annotated_type_with_anns(ctx, type_mirror, &anns, declaring_class_id)?;
    stash_annotated_type_argument_anns(&*ctx, at, type_arg_anns);
    Ok(Some(Value::Object(Some(at))))
}

/// `Parameter.getType()`, as a `Class` mirror вЂ” the fallback backing `Type`
/// for [`native_parameter_get_annotated_type`] when the declaring
/// executable's generic parameter type isn't available.
/// W7-26 — this returns `Result`, and the return type IS the fix.
///
/// W7-20's sharpest observation was that the same refusal was loud on four
/// rows and silent on one, and the only difference was the return type of the
/// helper it landed in. This was that helper: a bare `ObjectRef` gave it no
/// error channel, so an exception out of `Parameter.getType()` had nowhere to
/// go but the `ClassId(0)` mirror, and `Parameter.getAnnotatedType()` reflected
/// a parameter whose type was garbage. Both call sites are inside
/// `native_parameter_get_annotated_type`, which is a `MethodCallResult` and
/// already `?`s the `make_annotated_type_with_anns` two lines below — the
/// channel was always there, this helper just did not reach it.
pub fn parameter_erased_type_mirror(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    Ok(
        match ladder_rung(ctx.invoke_virtual(this, "getType", "()Ljava/lang/Class;", &[]))? {
            Some(Value::Object(Some(m))) => m,
            _ => ctx.get_class_mirror(cratonvm_types::ClassId::new(0)),
        },
    )
}

/// `Parameter.getAnnotatedType()Ljava/lang/reflect/AnnotatedType;`
///
/// Builds an AnnotatedType wrapping the parameter's type and carrying that
/// parameter's METHOD_FORMAL_PARAMETER TYPE_USE annotations. Prefers the
/// GENERIC parameter type (the `idx`'th element of the declaring
/// executable's `getGenericParameterTypes()`) over the erased one вЂ” see
/// `native_method_get_annotated_return_type` for why; falls back to the
/// erased `getType()` when the generic array is unavailable or the index is
/// out of range (e.g. a synthetic/mandated parameter with no Signature
/// entry).
pub fn native_parameter_get_annotated_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let exec = match ctx.get_field_by_name(this, "executable") {
        Value::Object(Some(e)) => e,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = match ctx.get_field_by_name(this, "index") {
        Value::Int(i) => i as usize,
        _ => 0,
    };
    let (declaring_class_id, anns, type_arg_anns) = match method_class_name_desc(ctx, exec) {
        Some((cid, name, desc)) => {
            let anns = ctx
                .method_parameter_type_annotations(cid, &name, &desc)
                .get(idx)
                .cloned()
                .unwrap_or_default();
            let type_arg_anns = ctx
                .method_parameter_type_argument_annotations(cid, &name, &desc)
                .get(idx)
                .cloned()
                .unwrap_or_default();
            (Some(cid), anns, type_arg_anns)        }
        None => (None, Vec::new(), Vec::new()),
    };
    // W7-26 — see `ladder_rung`. The erased fallback still covers every
    // non-throwing reason it already covered (no generic array, index out of
    // range for a synthetic/mandated parameter, a non-object element).
    // gen r4w3/rooting: `getGenericParameterTypes` runs Java; `this` is the
    // receiver of the erased fallback afterwards, so pin and re-read it.
    let this_pin = ctx.pin_native_root(this);
    let generic = ladder_rung(ctx.invoke_virtual(
        exec,
        "getGenericParameterTypes",
        "()[Ljava/lang/reflect/Type;",
        &[],
    ));
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let type_mirror = match generic? {
        Some(Value::Object(Some(arr))) if idx < ctx.array_length(arr) => {
            match ctx.get_array_element(arr, idx) {
                Value::Object(Some(m)) => m,
                _ => parameter_erased_type_mirror(ctx, this)?,
            }
        }
        _ => parameter_erased_type_mirror(ctx, this)?,
    };
    let at = make_annotated_type_with_anns(ctx, type_mirror, &anns, declaring_class_id)?;
    stash_annotated_type_argument_anns(&*ctx, at, type_arg_anns);
    Ok(Some(Value::Object(Some(at))))
}

/// `Executable.getAnnotatedParameterTypes()[Ljava/lang/reflect/AnnotatedType;`
///
/// One AnnotatedType per declared parameter, each carrying that parameter's
/// METHOD_FORMAL_PARAMETER TYPE_USE annotations. Prefers the GENERIC
/// parameter types (`getGenericParameterTypes()`) over the erased ones вЂ”
/// see `native_method_get_annotated_return_type` for why вЂ” falling back
/// per-element to the erased mirror when the generic array's length
/// doesn't match the erased one (e.g. synthetic/mandated parameters).
pub fn native_executable_get_annotated_parameter_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (declaring_class_id, per_param, per_param_type_args, count) =
        match method_class_name_desc(ctx, this) {
            Some((cid, name, desc)) => {
                let pta = ctx.method_parameter_type_annotations(cid, &name, &desc);
                let pta_args = ctx.method_parameter_type_argument_annotations(cid, &name, &desc);
                (Some(cid), pta, pta_args, count_method_params(&desc))
            }
            None => (None, Vec::new(), Vec::new(), 0),
        };
    // Resolve the erased parameter type mirrors once (fallback + length
    // reference for the generic array below).
    // W7-26 — see `ladder_rung`. An empty erased-mirror vector here is also the
    // LENGTH reference for the generic array below, so a swallowed exception
    // did not merely lose one type: it silently shortened
    // `getAnnotatedParameterTypes()` to zero elements.
    // gen r4w3/rooting: both `invoke_virtual`s run Java; pin `this` across
    // the first so the second's receiver is current.
    let this_pin = ctx.pin_native_root(this);
    let erased_res =
        ladder_rung(ctx.invoke_virtual(this, "getParameterTypes", "()[Ljava/lang/Class;", &[]));
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let erased_type_mirrors: Vec<ObjectRef> = match erased_res? {
        Some(Value::Object(Some(arr))) => {
            let n = ctx.array_length(arr);
            // gc-common w20-f: the fallback mirror was minted INSIDE the copy
            // loop, so a mirror allocation there left `arr` and every element
            // already collected at pre-allocation addresses. It is resolved
            // once, before the copy, with the array pinned across it (only
            // when some element actually needs it).
            let needs_fallback =
                (0..n).any(|i| !matches!(ctx.get_array_element(arr, i), Value::Object(Some(_))));
            let (arr, fallback): (ObjectRef, Option<ObjectRef>) = if needs_fallback {
                let arr_pin = ctx.pin_native_root(arr);
                let fallback = ctx.get_class_mirror(cratonvm_types::ClassId::new(0));
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(arr_pin);
                (arr, Some(fallback))
            } else {
                (arr, None)
            };
            (0..n)
                .filter_map(|i| match ctx.get_array_element(arr, i) {
                    Value::Object(Some(m)) => Some(m),
                    _ => fallback,
                })
                .collect()
        }
        _ => Vec::new(),
    };
    // gen r4w3/rooting: the erased mirrors are a bare `Vec<ObjectRef>` held
    // across the `getGenericParameterTypes` upcall below; pin each element
    // and re-read them all after it.
    let erased_pins: Vec<usize> = erased_type_mirrors
        .iter()
        .map(|m| ctx.pin_native_root(*m))
        .collect();
    let generic_res = ladder_rung(ctx.invoke_virtual(
        this,
        "getGenericParameterTypes",
        "()[Ljava/lang/reflect/Type;",
        &[],
    ));
    let erased_type_mirrors: Vec<ObjectRef> = erased_type_mirrors
        .iter()
        .zip(erased_pins.iter())
        .map(|(m, pin)| ctx.read_native_pin(*pin, *m))
        .collect();
    if let Some(&first) = erased_pins.first() {
        ctx.unpin_native_roots(first);
    }
    // W7-26 — see `ladder_rung`. The erased-mirror fallback below stays for
    // every non-throwing reason it already covered, including the deliberate
    // length-mismatch guard.
    let generic_type_mirrors: Vec<ObjectRef> = match generic_res? {
        Some(Value::Object(Some(arr))) if ctx.array_length(arr) == erased_type_mirrors.len() => {
            let n = ctx.array_length(arr);
            (0..n)
                .map(|i| match ctx.get_array_element(arr, i) {
                    Value::Object(Some(m)) => m,
                    _ => erased_type_mirrors[i],
                })
                .collect()
        }
        _ => erased_type_mirrors,
    };
    let n = generic_type_mirrors.len().max(count);
    let comp = ctx
        .class_id_by_name("java/lang/reflect/AnnotatedType")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    // GC-safety: `make_annotated_type_with_anns` allocates, once per turn. The
    // array being filled and every mirror in `generic_type_mirrors` are bare
    // Rust locals -- a `Vec<ObjectRef>` is not a GC root and nothing rewrites
    // its elements -- so from the second turn on both the store target and the
    // mirror handed to the builder are pre-GC addresses. Pin the array for the
    // loop and each mirror for the call that consumes it. (The earlier sweep
    // skipped this row because `pin_native_root` does not take a `Vec`; the
    // fix is to pin the ELEMENT.)
    //
    // gc-common w20-f: the mirrors are pinned BEFORE the array's allocation
    // (they were pinned after it, i.e. at their pre-allocation addresses), and
    // a failed builder releases every pin before its `?` exit.
    let mirror_pins: Vec<usize> = generic_type_mirrors
        .iter()
        .map(|m| ctx.pin_native_root(*m))
        .collect();
    let out = ctx.new_ref_array(comp, n);
    let out_pin = ctx.pin_native_root(out);
    let pin_base = mirror_pins.first().copied().unwrap_or(out_pin);
    for i in 0..n {
        let tm = match (generic_type_mirrors.get(i), mirror_pins.get(i)) {
            (Some(m), Some(pin)) => ctx.read_native_pin(*pin, *m),
            _ => ctx.get_class_mirror(cratonvm_types::ClassId::new(0)),
        };
        let empty = Vec::new();
        let anns = per_param.get(i).unwrap_or(&empty);
        let at = match make_annotated_type_with_anns(ctx, tm, anns, declaring_class_id) {
            Ok(at) => at,
            Err(e) => {
                ctx.unpin_native_roots(pin_base);
                return Err(e);
            }
        };
        // Stash this parameter's TYPE_ARGUMENT-level annotations (e.g. the
        // `@Valid` in `List<@Valid Person>`) alongside the AnnotatedType we
        // just built, so a later `getAnnotatedActualTypeArguments()` call on
        // it (if it turns out to be an `AnnotatedParameterizedType`) can
        // attach them to the right type-argument `AnnotatedType` -- see
        // `native_annotated_parameterized_type_get_annotated_actual_type_arguments`.
        if let Some(type_arg_anns) = per_param_type_args.get(i) {
            stash_annotated_type_argument_anns(&*ctx, at, type_arg_anns.clone());
        }
        let out = ctx.read_native_pin(out_pin, out);
        ctx.set_array_element(out, i, Value::Object(Some(at)));
    }
    let out = ctx.read_native_pin(out_pin, out);
    ctx.unpin_native_roots(pin_base);
    Ok(Some(Value::Object(Some(out))))
}

/// `Field.getAnnotatedType()Ljava/lang/reflect/AnnotatedType;`
///
/// Builds an AnnotatedType wrapping the field's type and carrying the
/// field's FIELD-target TYPE_USE annotations. Prefers the GENERIC field type
/// (`getGenericType()`) over the erased one вЂ” see
/// `native_method_get_annotated_return_type` for why.
pub fn native_field_get_annotated_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (declaring_class_id, anns, type_arg_anns) = match field_class_and_name(ctx, this) {
        Some((cid, name)) => (
            Some(cid),
            ctx.field_type_annotations(cid, &name),
            ctx.field_type_argument_annotations(cid, &name),
        ),
        None => (None, Vec::new(), Vec::new()),
    };
    // W7-26 — see `ladder_rung`. `Field.getAnnotatedType()` reflecting the
    // `ClassId(0)` mirror because `getGenericType()` threw is a wrong answer
    // dressed as a right one.
    // gen r4w3/rooting: the first upcall can collect, and `this` is the
    // receiver of the fallback upcall; pin it and re-read before reuse.
    let this_pin = ctx.pin_native_root(this);
    let type_mirror = match ladder_rung(ctx.invoke_virtual(
        this,
        "getGenericType",
        "()Ljava/lang/reflect/Type;",
        &[],
    )) {
        Ok(Some(Value::Object(Some(m)))) => Ok(m),
        Ok(_) => {
            let this = ctx.read_native_pin(this_pin, this);
            match ladder_rung(ctx.invoke_virtual(this, "getType", "()Ljava/lang/Class;", &[])) {
                Ok(Some(Value::Object(Some(m)))) => Ok(m),
                Ok(_) => Ok(ctx.get_class_mirror(cratonvm_types::ClassId::new(0))),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    };
    ctx.unpin_native_roots(this_pin);
    let type_mirror = type_mirror?;
    let at = make_annotated_type_with_anns(ctx, type_mirror, &anns, declaring_class_id)?;
    stash_annotated_type_argument_anns(&*ctx, at, type_arg_anns);
    Ok(Some(Value::Object(Some(at))))
}

/// SBR wave 5 (reflmeta): is `this` an `AnnotatedTypeBaseImpl` the REAL
/// `AnnotatedTypeFactory` built (under `--real-jdk`)?
///
/// The accessor natives below were written for the instances CratonVM
/// builds, which stash an `Annotation[]` in the `annotations` field the JDK
/// declares as a `Map`. A real instance carries the `Map` -- and, now that
/// `getTypeAnnotationBytes0` and `ConstantPool` answer under `--real-jdk`,
/// real annotations in it: the real `TypeVariable.getAnnotatedBounds()`,
/// `Field.getAnnotatedType()`, `Executable.getAnnotatedParameterTypes()` and
/// `Parameter.getAnnotatedType()` build these. Reading such an instance as
/// "no stashed array, so empty" (the old answer) lost every annotation, so for
/// it these natives run the JDK's own body instead
/// ([`annotated_type_real_body`]). `--compatible` never takes this branch
/// (byte-for-byte unchanged).
pub fn annotated_type_is_real_shape(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if !ctx.is_jdk_only() {
        return false;
    }
    match ctx.get_field_by_name(this, "annotations") {
        Value::Object(Some(a)) => !ctx.object_is_array(a),
        _ => false,
    }
}

/// The JDK's own body of `class.name desc` for a real-shape receiver (see
/// [`annotated_type_is_real_shape`]) -- `invoke_special_bytecode_only`, since
/// a plain dispatch would re-enter the native that called it.
pub fn annotated_type_real_body(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    ctx.invoke_special_bytecode_only(class, name, desc, args)
}

pub const ANNOTATED_TYPE_BASE_IMPL: &str =
    "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl";

/// `AnnotatedType.getDeclaredAnnotations()` / `getAnnotations()` override for
/// the `AnnotatedTypeBaseImpl` instances CratonVM constructs. Returns the
/// stashed `Annotation[]` (or an empty array).
pub fn native_annotated_type_get_declared_annotations(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if annotated_type_is_real_shape(ctx, this) {
        // Both registrations share this body; the two JDK bodies are equal
        // (`getAnnotations()` is `getDeclaredAnnotations()`).
        return annotated_type_real_body(
            ctx,
            ANNOTATED_TYPE_BASE_IMPL,
            "getDeclaredAnnotations",
            "()[Ljava/lang/annotation/Annotation;",
            args,
        );
    }
    let stashed = annotated_type_stashed_anns(ctx, this);
    if let Some((arr, len)) = stashed {
        if len > 0 {
            // The field holds an actual `Annotation[]` (length>0 в‡’ array kind);
            // return it directly.
            return Ok(Some(Value::Object(Some(arr))));
        }
    }
    let comp = annotation_component_class_id(ctx);
    let empty = ctx.new_ref_array(comp, 0);
    Ok(Some(Value::Object(Some(empty))))
}

/// `AnnotatedType.getAnnotation(Class)` override вЂ” scans the stashed proxy
/// array for one whose `annotationType()` matches the requested class.
pub fn native_annotated_type_get_annotation(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if annotated_type_is_real_shape(ctx, this) {
        return annotated_type_real_body(
            ctx,
            ANNOTATED_TYPE_BASE_IMPL,
            "getAnnotation",
            "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
            args,
        );
    }
    let want = match args.get(1) {
        Some(Value::Object(Some(c))) => mirror_class_id(ctx, *c),
        _ => None,
    };
    let want = match want {
        Some(w) => w,
        None => return Ok(Some(Value::Object(None))),
    };
    if let Some((arr, n)) = annotated_type_stashed_anns(ctx, this) {
        // gen r4w3/rooting: every `annotationType()` below runs Java; the
        // stashed array is pinned for the scan (re-read per element) and each
        // proxy across its own upcall, since a match is returned.
        let arr_pin = ctx.pin_native_root(arr);
        for i in 0..n {
            let arr = ctx.read_native_pin(arr_pin, arr);
            if let Value::Object(Some(proxy)) = ctx.get_array_element(arr, i) {
                // Annotation instances are materialized as ordinary JDK dynamic
                // proxies here. Do not assume the legacy synthetic
                // `AnnotationProxy` field layout: reading slot 1 from a dynamic
                // proxy is invalid and turns a valid type-use annotation into a
                // false negative. The public Annotation contract supplies the
                // precise type mirror for both representations.
                // W7-26 — `ladder_rung`, then match on the VALUE. Found by the
                // sweep that followed the four `getAnnotation` sites: this is
                // the fifth instance of the same species and the only one
                // outside the `Class`/`Field` pair. `annotationType()` here is
                // a real virtual dispatch into a JDK dynamic proxy's
                // invocation handler, so it can genuinely throw, and swallowing
                // that turned `AnnotatedType.getAnnotation(X)` into null —
                // which reads as "no such type-use annotation".
                //
                // `ladder_rung` rather than a bare `?` because this is a SCAN,
                // not a build: unlike the four `Class`/`Field` sites, there is
                // a next element to try, so a stashed object that has no
                // `annotationType` at all (`InternalError`) should still be
                // skipped exactly as it was before. Only a real pending
                // exception aborts. A non-Class or void return is likewise
                // still just "not this element".
                let proxy_pin = ctx.pin_native_root(proxy);
                let ann_type = ladder_rung(ctx.invoke_virtual(
                    proxy,
                    "annotationType",
                    "()Ljava/lang/Class;",
                    &[],
                ));
                let proxy = ctx.read_native_pin(proxy_pin, proxy);
                ctx.unpin_native_roots(proxy_pin);
                let ann_type = match ann_type {
                    Ok(v) => v,
                    Err(e) => {
                        ctx.unpin_native_roots(arr_pin);
                        return Err(e);
                    }
                };
                if let Some(Value::Object(Some(tm))) = ann_type {
                    if mirror_class_id(ctx, tm) == Some(want) {
                        ctx.unpin_native_roots(arr_pin);
                        return Ok(Some(Value::Object(Some(proxy))));
                    }
                }
            }
        }
        ctx.unpin_native_roots(arr_pin);
    }
    Ok(Some(Value::Object(None)))
}

/// `AnnotatedType.getAnnotatedOwnerType()` override for CratonVM-built
/// `AnnotatedTypeBaseImpl` instances.
///
/// The real JDK implementation reconstructs the owner from type-annotation
/// location state. CratonVM constructs these wrappers directly from an already
/// reified `Type` and does not populate the full location graph, so falling
/// through into that bytecode can confuse reflective readers such as ByteBuddy's
/// JavaDispatcher during Mockito inline mock generation. For the top-level
/// types used by `java.nio.file.Path` this is simply null; for parameterized
/// nested types, preserve the useful case by wrapping `ParameterizedType`'s
/// owner type when one exists.
pub fn native_annotated_type_get_annotated_owner_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if annotated_type_is_real_shape(ctx, this) {
        return annotated_type_real_body(
            ctx,
            ANNOTATED_TYPE_BASE_IMPL,
            "getAnnotatedOwnerType",
            "()Ljava/lang/reflect/AnnotatedType;",
            args,
        );
    }
    let backing_type = match ctx.get_field_by_name(this, "type") {
        Value::Object(Some(t)) => t,
        _ => return Ok(Some(Value::Object(None))),
    };

    let backing_cid = ctx.class_id_of_object(backing_type);
    let backing_name = ctx.class_name_of_id(backing_cid).unwrap_or_default();

    if matches!(
        backing_name.as_str(),
        "java/lang/reflect/ParameterizedType"
            | "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl"
    ) {
        // W7-26 — see `ladder_rung`. A null owner is a legitimate answer for a
        // top-level type, so an exception swallowed here was indistinguishable
        // from "this type has no owner".
        if let Some(Value::Object(Some(owner))) = ladder_rung(ctx.invoke_virtual(
            backing_type,
            "getOwnerType",
            "()Ljava/lang/reflect/Type;",
            &[],
        ))? {
            let at = make_annotated_type(ctx, owner);
            return Ok(Some(Value::Object(Some(at))));
        }
    }

    if backing_name == "java/lang/Class" {
        // W7-26 — same shape, same null-is-legitimate hazard.
        if let Some(Value::Object(Some(owner))) = ladder_rung(ctx.invoke_virtual(
            backing_type,
            "getDeclaringClass",
            "()Ljava/lang/Class;",
            &[],
        ))? {
            let at = make_annotated_type(ctx, owner);
            return Ok(Some(Value::Object(Some(at))));
        }
    }

    Ok(Some(Value::Object(None)))
}

/// `AnnotatedParameterizedType.getAnnotatedActualTypeArguments()
/// [Ljava/lang/reflect/AnnotatedType;` override.
///
/// Real JDK's `AnnotatedParameterizedTypeImpl` computes this from
/// `getTypeAnnotationBytes0`-derived bookkeeping, which CratonVM never
/// populates (that native is stubbed to null -- see the module-level notes
/// on `make_annotated_type`), so the real-bytecode path returns type
/// arguments with no annotations. That breaks Spring's
/// `HandlerMethod.MethodValidationInitializer.getContainerElementAnnotations`,
/// which relies on this call to find `@Valid`/`@Constraint` annotations
/// nested in a parameter's type arguments (e.g. `List<@Valid Person>`).
///
/// This override: reads the wrapped `Type` (stashed in the `type` field by
/// [`make_annotated_type`] / [`make_annotated_type_with_anns`]), calls its
/// `getActualTypeArguments()` (real JDK `ParameterizedTypeImpl`, works fine
/// since generic signatures are already parsed correctly), and wraps each
/// argument as a plain `AnnotatedType` -- attaching the TYPE_ARGUMENT-level
/// annotations stashed by the caller (return type / parameter / field /
/// class-supertype natives, via [`take_annotated_type_argument_anns`]) when
/// available. Each argument's own `children` subtree (if non-empty) is
/// re-stashed against the newly built `AnnotatedType`, so calling this same
/// native AGAIN on that nested result (e.g. HotSpot's
/// `AnnotatedParameterizedType.getAnnotatedActualTypeArguments()` applied a
/// second time, as Hibernate Validator's `ValueExtractorResolver` does for
/// `ValueExtractor<ArgumentValue<@ExtractedValue ?>>`) finds the next
/// nesting level's annotations rather than dead-ending after one level.
/// AnnotatedTypes not built with any per-argument annotations (no side-table
/// entry) simply get argument wrappers with no annotations, matching the
/// prior (gap) behavior.
pub fn native_annotated_parameterized_type_get_annotated_actual_type_arguments(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if annotated_type_is_real_shape(ctx, this) {
        return annotated_type_real_body(
            ctx,
            "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedParameterizedTypeImpl",
            "getAnnotatedActualTypeArguments",
            "()[Ljava/lang/reflect/AnnotatedType;",
            args,
        );
    }
    let per_arg_anns = take_annotated_type_argument_anns(&*ctx, this);

    let backing_type = match ctx.get_field_by_name(this, "type") {
        Value::Object(Some(t)) => t,
        _ => {
            let comp = ctx
                .class_id_by_name("java/lang/reflect/AnnotatedType")
                .unwrap_or(cratonvm_types::ClassId::new(0));
            return Ok(Some(Value::Object(Some(ctx.new_ref_array(comp, 0)))));
        }
    };

    // W7-26 — see `ladder_rung`. The empty-array fallback is the documented
    // no-side-table behaviour and stays; what changes is that an exception out
    // of `getActualTypeArguments()` no longer reads as "this parameterized type
    // has no arguments", which for an `AnnotatedParameterizedType` is a
    // contradiction a caller cannot detect.
    let type_args = match ladder_rung(ctx.invoke_virtual(
        backing_type,
        "getActualTypeArguments",
        "()[Ljava/lang/reflect/Type;",
        &[],
    ))? {
        Some(Value::Object(Some(arr))) => arr,
        _ => {
            let comp = ctx
                .class_id_by_name("java/lang/reflect/AnnotatedType")
                .unwrap_or(cratonvm_types::ClassId::new(0));
            return Ok(Some(Value::Object(Some(ctx.new_ref_array(comp, 0)))));
        }
    };
    let n = ctx.array_length(type_args);
    let comp = ctx
        .class_id_by_name("java/lang/reflect/AnnotatedType")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let out = ctx.new_ref_array(comp, n);
    let empty = TypeArgAnnotations::default();
    // gen r4w3/rooting: `make_annotated_type_with_anns` is a GC point once per
    // element; pin the source and result arrays and re-read them each turn.
    let src_pin = ctx.pin_native_root(type_args);
    let out_pin = ctx.pin_native_root(out);
    for i in 0..n {
        let type_args = ctx.read_native_pin(src_pin, type_args);
        let tm = match ctx.get_array_element(type_args, i) {
            Value::Object(Some(m)) => m,
            _ => continue,
        };
        let node = per_arg_anns
            .as_ref()
            .and_then(|v| v.get(i))
            .unwrap_or(&empty);
        let at = match make_annotated_type_with_anns(ctx, tm, &node.anns, None) {
            Ok(at) => at,
            Err(e) => {
                ctx.unpin_native_roots(src_pin);
                return Err(e);
            }
        };
        if !node.children.is_empty() {
            stash_annotated_type_argument_anns(&*ctx, at, node.children.clone());
        }
        let out = ctx.read_native_pin(out_pin, out);
        ctx.set_array_element(out, i, Value::Object(Some(at)));
    }
    let out = ctx.read_native_pin(out_pin, out);
    ctx.unpin_native_roots(src_pin);
    Ok(Some(Value::Object(Some(out))))
}

/// Build a non-null, empty `java.security.Permissions` collection.
///
/// We allocate the real-layout object and run its no-arg constructor so the
/// internal `permsMap` is initialised вЂ” this makes `PermissionCollection`'s
/// `elements()` / `toString()` work and renders as HotSpot's
/// `java.security.Permissions@HASH ( )` for an app class with no policy
/// grants. The constructor run is best-effort: even if it fails the object is
/// still non-null, which is the contract callers (`getPermissions()`) rely on.
pub fn build_empty_permissions(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let perms = crate::try_alloc_concurrent_synthetic(ctx, "java/security/Permissions", 2)?;
    // The `<init>` below runs Java and allocates, so it can relocate `perms` —
    // and this function was returning the address it started with. The callee
    // gets a rooted copy through the invoke's own argument handling; this local
    // is what the CALLER keeps, and nothing rooted it.
    //
    // MEASURED on BindableTests under `CRATONVM_DBG_GC_STRESS=262144`:
    // `[deadref-pin]` fired on `populate_protection_domain_fields`' pin of this
    // return value — the pin being handed an address that already named no live
    // object.
    let perms_pin = ctx.pin_native_root(perms);
    let _ = ctx.invoke(
        "java/security/Permissions",
        "<init>",
        "()V",
        &[Value::Object(Some(perms))],
    );
    let perms = ctx.read_native_pin(perms_pin, perms);
    ctx.unpin_native_roots(perms_pin);
    Ok(perms)
}

/// Populate a freshly-allocated `java.security.ProtectionDomain` with faithful
/// `codesource` / `classloader` / `permissions` / `principals` fields.
///
/// Writes BOTH the synthetic field layout (slot 0 = codesource, 1 =
/// permissions, 2 = classloader, 3 = principals вЂ” see `class_manager.rs`) AND
/// the real-JDK layout (slot 0 = codesource, 1 = classloader, 2 = principals,
/// 3 = permissions) by name. The slot writes land first; the by-name writes
/// run last and are authoritative, so a real-JDK-loaded `ProtectionDomain`
/// ends up with each value in the correct field regardless of which layout the
/// object actually has.
///
/// `classloader` is the class's defining loader (matching
/// `Class.getClassLoader()`); previously this slot was left null or вЂ” worse вЂ”
/// clobbered with the permissions object (SBR-13).
pub fn populate_protection_domain_fields(
    ctx: &mut dyn NativeContext,
    pd: ObjectRef,
    codesource: Value,
    classloader: Value,
) -> Result<(), MethodCallFailed> {
    // GC-SAFETY: `build_empty_permissions`/`new_array` below can each
    // trigger a moving GC; `pd` (the receiver) and the caller-supplied
    // `codesource`/`classloader` are all used again afterward. Pin
    // everything up front and re-read before use.
    let pd_pin = ctx.pin_native_root(pd);
    let cs_obj = match codesource {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let cs_pin = cs_obj.map(|o| ctx.pin_native_root(o));
    let cl_obj = match classloader {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let cl_pin = cl_obj.map(|o| ctx.pin_native_root(o));

    // `perms` is allocated and then `new_array` allocates again before it is
    // read, so a moving young collection between the two relocates it and the
    // `set_field_by_name("permissions", ...)` below stored the pre-move
    // address. Every other reference in this function is pinned across exactly
    // this hazard; `perms` was the one that was not.
    //
    // MEASURED on BindableTests under `CRATONVM_DBG_GC_STRESS=262144`:
    // `[deadref-store]` named this function storing a value that named no live
    // object, reached from `native_class_get_protection_domain0`.
    let perms = build_empty_permissions(ctx);
    let perms_pin = perms.as_ref().ok().map(|o| ctx.pin_native_root(*o));
    let principals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    let perms = match (perms, perms_pin) {
        (Ok(o), Some(p)) => Ok(ctx.read_native_pin(p, o)),
        (other, _) => other,
    };

    let pd = ctx.read_native_pin(pd_pin, pd);
    let codesource = match (cs_obj, cs_pin) {
        (Some(o), Some(p)) => Value::Object(Some(ctx.read_native_pin(p, o))),
        _ => codesource,
    };
    let classloader = match (cl_obj, cl_pin) {
        (Some(o), Some(p)) => Value::Object(Some(ctx.read_native_pin(p, o))),
        _ => classloader,
    };
    let perms_v = Value::Object(Some(perms?));
    let principals_v = Value::Object(Some(principals));

    // By NAME only, which is right on both layouts.
    //
    // This used to write slots 0..3 first, in the order
    // `(codesource, permissions, classloader, principals)` — the JDK
    // CONSTRUCTOR's argument order, not the field declaration order, which is
    // `(codesource, classloader, principals, permissions)`. On a real
    // `java.security.ProtectionDomain` that put the permissions in
    // `classloader`, the loader in `principals` and the principals in
    // `permissions`. All four are references, so the overlay hunter's
    // value-tag test could never see it; the L4 shadow-layout diff, which
    // compares by name, is what named it.
    //
    // It was never observable, because the by-name pass below writes the same
    // four fields and ran last — the deleted comment said "authoritative —
    // runs last", which is a load-bearing ordering nobody could see from the
    // call site. So the raw pass was dead weight on a real layout and
    // redundant on a fabricated one (the model names all four there too), and
    // removing it deletes the ordering dependency along with the wrong writes.
    ctx.set_field_by_name(pd, "codesource", codesource);
    ctx.set_field_by_name(pd, "permissions", perms_v);
    ctx.set_field_by_name(pd, "classloader", classloader);
    ctx.set_field_by_name(pd, "principals", principals_v);
    ctx.unpin_native_roots(pd_pin);
    Ok(())
}

/// `java/lang/Class.getClassFileVersion0()I`
///
/// Returns the class file version number. The JDK encodes this as
/// `(major << 16) | minor`, but many callers just want the major version.
/// We return the major version number (e.g., 65 for Java 21, 69 for Java 25).
pub fn native_class_get_class_file_version(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Interpreter round i5 wave 106, lane L3 (`L3W106ClassFileVersion`):
    // HotSpot's `JVM_GetClassFileVersion` answers a primitive (and `void`)
    // `JVM_CLASSFILE_MAJOR_VERSION` (69 on JDK 25) with minor 0, and any
    // other class `(minor << 16) | major` -- a preview class file's
    // `0xFFFF` minor included, which `ReflectionFactory.classFileFormatVersion`
    // reads to fall back to the latest format. This answered the major alone,
    // and 65 for a mirror it could not map (a primitive among them).
    if class_mirror_is_primitive(ctx, this) {
        return Ok(Some(Value::Int(JDK25_CLASSFILE_MAJOR_VERSION)));
    }
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(JDK25_CLASSFILE_MAJOR_VERSION))),
    };

    let major = i32::from(ctx.class_file_version(class_id));
    let minor = i32::from(ctx.class_file_minor_version(class_id));
    Ok(Some(Value::Int((minor << 16) | major)))
}

/// `JVM_CLASSFILE_MAJOR_VERSION` of the JDK this VM targets (25).
pub const JDK25_CLASSFILE_MAJOR_VERSION: i32 = 69;

// ---------------------------------------------------------------------------
// T19.N1: java/lang/Class security-related natives
// ---------------------------------------------------------------------------
//
// Three natives that expose a class's `CodeSource` (installed by the class
// loader at class-define time) to reflective callers:
//
//   * `getProtectionDomain0()` returns a lightweight `ProtectionDomain`
//     whose `CodeSource` carries the class's load URL and any signer
//     certificates.  Bootstrap / synthetic classes (empty or `class:`
//     code-base) return null вЂ” matching HotSpot for boot classes.
//
//   * `getSigners()` returns the raw signer-certificate blocks as
//     `byte[]` elements of an `Object[]`, or null for unsigned classes.
//     Real JDK returns `Certificate[]` вЂ” we approximate with byte arrays
//     since our synthetic-JDK surface doesn't fully implement
//     `java.security.cert.Certificate`.
//
//   * `setSigners([Ljava/lang/Object;)V` is a documented no-op at the
//     moment (we do not track mutable per-class signers).  A
//     `tracing::debug!` fires so any runtime dependency on the setter
//     surfaces in logs rather than silently corrupting state.

/// `java/lang/Class.getProtectionDomain0()Ljava/security/ProtectionDomain;`
///
/// Returns the `ProtectionDomain` for this class.
///
/// * For classes loaded from the bootstrap loader (no CodeSource URL, or a
///   synthetic `class:...` placeholder URL), returns null вЂ” matching HotSpot
///   where the boot loader's PD is `null`.
/// * For classes with a real CodeSource URL (typically `file:/...` from a
///   JAR or directory classpath entry), allocates a minimal-viable
///   `java/security/ProtectionDomain` with a 1-field `CodeSource` child
///   holding the URL and any signer certs.  Permissions and class loader
///   fields are left null which the JDK spec treats as "all permissions,
///   bootstrap loader" вЂ” the safest default for a non-enforcing policy.
pub fn native_class_get_protection_domain0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    // A nestmate hidden class's domain is its host's, whatever was installed
    // at its define (see `hidden_class_host_domain`).
    if let Some(pd) = hidden_class_host_domain(ctx, class_id)? {
        return Ok(Some(pd));
    }
    let this = obj_arg(args, 0)?;
    if let Some(pd) = installed_protection_domain(ctx, this) {
        return Ok(Some(Value::Object(Some(pd))));
    }
    // HotSpot's `getProtectionDomain0` answers null for EVERY class the
    // bootstrap loader defines -- see `is_boot_defined_class` -- and for a
    // generated proxy (`defined_without_domain`).
    if is_boot_defined_class(ctx, class_id, args)? || defined_without_domain(&*ctx, class_id) {
        return Ok(Some(Value::Object(None)));
    }
    protection_domain_from_code_source(ctx, class_id, args)
}

/// The `ProtectionDomain` `Class.getProtectionDomain0` answers for `mirror`
/// (`null` for a class the bootstrap loader defined): the `protectionDomain`
/// a redefinition or retransformation hands the `ClassFileTransformer`s, as
/// HotSpot's `JvmtiClassFileLoadHookPoster` passes the class's own domain
/// (`vm/src/runtime/instrument.rs` `run_chain_over_bytes`; interpreter round i1
/// wave 28). May allocate: the caller holds `mirror` pinned.
pub fn protection_domain_of_mirror(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
) -> Option<ObjectRef> {
    match native_class_get_protection_domain0(ctx, &[Value::Object(Some(mirror))]) {
        Ok(Some(Value::Object(domain))) => domain,
        _ => None,
    }
}

/// The `ProtectionDomain` a `defineClass*` native stored in the mirror's
/// `protectionDomain` field, which HotSpot answers for both
/// `getProtectionDomain` and `getProtectionDomain0`. `None` for a class the VM
/// defined itself from the class path, which keeps the code-source answer
/// below. Both modes since interpreter round i1 wave 47, lane L6 (the define
/// natives install it in `--compatible` too).
pub fn installed_protection_domain(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
) -> Option<ObjectRef> {
    match ctx.get_field_by_name(mirror, "protectionDomain") {
        Value::Object(Some(pd)) => Some(pd),
        _ => None,
    }
}

/// A class the JDK defines with a NULL domain although its loader is not the
/// bootstrap loader: a `java.lang.reflect.Proxy` class (`ProxyBuilder`
/// passes `null` to `JLA.defineClass`), so HotSpot answers the JDK's
/// all-permission domain -- null `CodeSource`, null loader -- where this VM
/// synthesised a `file:/.../$Proxy0.class` CodeSource of the proxy's loader
/// (interpreter round i1 wave 49, lane L6; both modes).
pub fn defined_without_domain(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    // An array class too: HotSpot never gives one a domain, whatever its
    // component (`Foo[].class.getProtectionDomain() ==
    // String.class.getProtectionDomain()`), where this VM built a `file:.`
    // domain of the component's loader. `class_id` is the array CLASS's own
    // id here (a mirror's), not an array object's header id.
    ctx.class_name_of_id(class_id)
        .is_some_and(|n| n.starts_with('['))
        || crate::reflect_annotations::proxy_class_is_generated(ctx, class_id)
}

/// A HIDDEN class with no installed domain whose nest host is another class
/// (a lambda proxy, `Foo$$Lambda/0x..`, the VM's own lambda doors define
/// without a domain): the host's domain. HotSpot defines every hidden class
/// with its lookup class's domain (`Lookup.lookupClassProtectionDomain`), and a
/// NESTMATE hidden class's nest host IS that lookup class, so
/// `lambda.getClass().getProtectionDomain() == Host.class.getProtectionDomain()`
/// there; this VM built a fresh `file:.` domain per call (interpreter round i1
/// wave 49, lane L6; both modes). `None` for every other class.
pub fn hidden_class_host_domain(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
) -> Result<Option<Value>, MethodCallFailed> {
    // The VM's synthetic lambda proxies (ids from 0x8000_0000, which
    // `Class.isHidden()` reports hidden through their recorded host) first.
    let host_name = if class_id.as_u32() >= 0x8000_0000 {
        ctx.lambda_proxy_host(class_id)
    } else {
        None
    };
    let host_name = match host_name {
        Some(host) => host,
        None if ctx.is_class_hidden(class_id) => match ctx.nest_host_name(class_id) {
            Some(host) => host,
            None => return Ok(None),
        },
        None => return Ok(None),
    };
    let host = ctx
        .class_id_by_name_near(&host_name, class_id)
        .or_else(|| ctx.class_id_by_name_delegated(&host_name));
    if crate::nbflags().dbg_sbload {
        eprintln!("[DBG_SBLOAD] hidden domain {class_id:?} host={host_name} -> {host:?}");
    }
    let Some(host) = host else {
        return Ok(None);
    };
    if host == class_id || ctx.is_class_hidden(host) {
        return Ok(None);
    }
    let host_mirror = ctx.get_class_mirror(host);
    native_class_get_protection_domain(ctx, &[Value::Object(Some(host_mirror))])
}

/// `java/lang/Class.getProtectionDomain()Ljava/security/ProtectionDomain;` --
/// the public surface, registered apart from `getProtectionDomain0` because
/// the two disagree about a bootstrap class.
///
/// A bootstrap-defined class runs the JDK's OWN body (both modes since
/// interpreter round i1 wave 47). On
/// JDK 25 that reads the `protectionDomain` field -- null for a class the
/// bootstrap loader defines -- and answers `Class$Holder.allPermDomain`, a
/// ProtectionDomain whose `getCodeSource()` is **null**; on JDK 21 the body
/// asks `getProtectionDomain0`, which answers null above, and builds the same
/// all-permission domain. The native used to hand every boot class a synthetic
/// `file:/runtime-defined/<name>.class` CodeSource instead, which is exactly
/// what `jdk.internal.jrtfs.SystemImage.findHome()` refuses: a non-null
/// CodeSource there means "loaded from `lib/jrt-fs.jar`", so the real `jrt:/`
/// filesystem died in `<clinit>` with `InternalError: ... unexpected path`.
///
/// Every other class keeps the CodeSource-backed answer.
pub fn native_class_get_protection_domain(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        // A primitive mirror (`int.class`): no class id, and HotSpot answers
        // the JDK body's all-permission domain, never null (interpreter round
        // i1 wave 49, lane L6; both modes).
        None => {
            return ctx.invoke_virtual_bytecode_only(
                this,
                "getProtectionDomain",
                "()Ljava/security/ProtectionDomain;",
                &[],
            )
        }
    };
    // First, as in `getProtectionDomain0`: a nestmate hidden class's domain is
    // its host's. It allocates only when it answers, so `this` stays current.
    if let Some(pd) = hidden_class_host_domain(ctx, class_id)? {
        return Ok(Some(pd));
    }
    if let Some(pd) = installed_protection_domain(ctx, this) {
        return Ok(Some(Value::Object(Some(pd))));
    }
    if is_boot_defined_class(ctx, class_id, args)? || defined_without_domain(&*ctx, class_id) {
        return ctx.invoke_virtual_bytecode_only(
            this,
            "getProtectionDomain",
            "()Ljava/security/ProtectionDomain;",
            &[],
        );
    }
    protection_domain_from_code_source(ctx, class_id, args)
}

/// True when `class_id` was defined by the BOOTSTRAP loader. HotSpot gives
/// such a class no protection domain at all (`getProtectionDomain0` is null,
/// `getProtectionDomain` is the shared all-permission domain with a null
/// CodeSource) -- measured on Temurin 25.0.4 for `java.lang.String`,
/// `java.lang.Object` and `jdk.internal.jrtfs.SystemImage` alike. Both modes
/// since interpreter round i1 wave 47, lane L6: `--compatible` answered a
/// synthesised CodeSource (probe
/// `L6W47BootClassDomain.java`).
///
/// Both halves are required: this VM defines platform-loader modules
/// (`java.sql`, `java.xml.crypto`, ...) under loader id 0 as well, yet their
/// Java-visible `getClassLoader()` is the platform loader and HotSpot hands
/// them a real `jrt:/<module>` CodeSource. Only a class whose
/// `getClassLoader()` really is null takes the boot answer.
pub fn is_boot_defined_class(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    args: &[Value],
) -> Result<bool, MethodCallFailed> {
    if ctx.loader_id_of_class(class_id) != 0 {
        return Ok(false);
    }
    Ok(matches!(
        native_class_get_class_loader(ctx, args)?,
        Some(Value::Object(None))
    ))
}

/// The CodeSource-backed ProtectionDomain both registrations above answer for
/// any class that is not a `--real-jdk` bootstrap class.
///
/// `--real-jdk`, a class the application loader defined from the class path:
/// the one domain per code base that [`app_class_path_domain`] also hands the
/// load-time transformers, so the domain a transformer saw is the object
/// `getProtectionDomain()` answers, and every class of one jar shares it, as
/// on HotSpot (both modes since wave 29). Every other class gets a fresh
/// domain per call as before.
pub fn protection_domain_from_code_source(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(code_base) = domain_code_base(ctx, class_id) else {
        return Ok(Some(Value::Object(None)));
    };
    let certs = ctx.class_code_source_certs(class_id);
    // Every mode since wave 29, so the domain a load-time transformer saw is
    // the class's own in `--compatible` too.
    let cached = ctx.loader_id_of_class(class_id) == APPLICATION_LOADER_ID
        && is_class_path_code_base(&code_base);
    if !cached {
        let pd = build_code_source_domain(ctx, code_base, certs, DomainLoader::OfMirror(this))?;
        return Ok(Some(Value::Object(Some(pd))));
    }
    if let Some(pd) = crate::classloader::app_code_source_domain(
        crate::classloader::loader_tables(ctx),
        &code_base,
    ) {
        return Ok(Some(Value::Object(Some(pd))));
    }
    let pd = build_code_source_domain(ctx, code_base.clone(), certs, DomainLoader::OfMirror(this))?;
    let pd = crate::classloader::publish_app_code_source_domain(
        crate::classloader::loader_tables(ctx),
        &code_base,
        pd,
    );
    Ok(Some(Value::Object(Some(pd))))
}

/// `NativeContext::loader_id_of_class`'s answer for the application loader.
pub const APPLICATION_LOADER_ID: i32 = 2;

/// A code base the class path produced, rather than the per-class
/// `file:/runtime-defined/<name>.class` stand-in the definer gives a class
/// generated in memory (`ClassManager::define_class_with_options`): one domain
/// per such name would grow the cache with every generated class.
pub fn is_class_path_code_base(code_base: &str) -> bool {
    !code_base.starts_with("file:/runtime-defined/")
}

/// `--real-jdk`: the `ProtectionDomain` the application loader gives a class it
/// defines from the class-path entry `code_base`, before the class exists: the
/// `protectionDomain` HotSpot's `JvmtiClassFileLoadHookPoster` hands the
/// load-time `ClassFileTransformer`s (`vm/src/runtime/instrument.rs`
/// `pre_transform_for_load`). The class's `getProtectionDomain()` answers the
/// same object later (see [`protection_domain_from_code_source`]). `None`
/// outside `--real-jdk`, for a generated class's stand-in code base, or when
/// the domain cannot be built. May allocate.
pub fn app_class_path_domain(
    ctx: &mut dyn NativeContext,
    code_base: &str,
    certs: Vec<Vec<u8>>,
) -> Option<ObjectRef> {
    // Every mode since interpreter round i1 wave 29: `--compatible` handed the
    // transformers `null`, so JaCoCo's default filter skipped every
    // class-path class and a covered run recorded nothing (a HotSpot
    // divergence, `L3/L3W28TransformerDomain`).
    if code_base.is_empty() || !is_class_path_code_base(code_base) {
        return None;
    }
    if let Some(pd) =
        crate::classloader::app_code_source_domain(crate::classloader::loader_tables(ctx), code_base)
    {
        return Some(pd);
    }
    let loader = crate::classloader::get_or_create_app_loader(ctx).ok()?;
    let pd = build_code_source_domain(
        ctx,
        code_base.to_string(),
        certs,
        DomainLoader::Given(loader),
    )
    .ok()?;
    Some(crate::classloader::publish_app_code_source_domain(
        crate::classloader::loader_tables(ctx),
        code_base,
        pd,
    ))
}

/// Where [`build_code_source_domain`] finds the domain's `classLoader`.
pub enum DomainLoader {
    /// Ask `Class.getClassLoader()` of this mirror.
    OfMirror(ObjectRef),
    /// This loader object.
    Given(ObjectRef),
}

/// The code base `class_id`'s domain is built from, `None` when it has none
/// (a bootstrap or synthetic class).
pub fn domain_code_base(ctx: &mut dyn NativeContext, class_id: ClassId) -> Option<String> {
    // A class of a non-boot image module (defined in the boot namespace here,
    // reported as the platform or application loader's): HotSpot's
    // `jrt:/<module>`, not the synthesised `file:/runtime-defined/...`
    // (interpreter round i1 wave 50, lane L5; `PdProbe.java`).
    if ctx.loader_id_of_class(class_id) == 0 {
        if let Some(name) = ctx.class_name_of_id(class_id) {
            if let Some(jrt) = crate::classloader::image_module_code_base(ctx, &name) {
                return Some(jrt);
            }
        }
    }
    let mut code_base = ctx.class_code_base(class_id).unwrap_or_default();
    // `Class.code_source` can be missing on some mirror edges; Spring Boot's
    // `Launcher.createArchive` needs a real `file:` URL. Mirror
    // `Class.getProtectionDomain` (lib.rs): fall back to the first
    // `java.class.path` entry (the executable JAR for `java -jar`).
    if code_base.is_empty() || code_base.starts_with("class:") {
        code_base = ctx
            .get_system_property("java.class.path")
            .and_then(|cp| {
                let sep = if cfg!(windows) { ';' } else { ':' };
                cp.split(sep)
                    .next()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();
    }
    if code_base.is_empty() || code_base.starts_with("class:") {
        return None;
    }
    Some(code_base)
}

/// Build a `ProtectionDomain` whose `CodeSource` is `code_base` with `certs`,
/// and whose `classLoader` comes from `loader`.
pub fn build_code_source_domain(
    ctx: &mut dyn NativeContext,
    code_base: String,
    certs: Vec<Vec<u8>>,
    loader: DomainLoader,
) -> Result<ObjectRef, MethodCallFailed> {
    let (this, loader_of_mirror) = match loader {
        DomainLoader::OfMirror(mirror) => (mirror, true),
        DomainLoader::Given(loader) => (loader, false),
    };

    // FIX: Preserve the original, untransformed code-base URL string (e.g.
    // `file:/opt/app.jar`). The transforms below rewrite `code_base` into a
    // HotSpot-style filesystem path for the synthetic `java.net.URL` object,
    // which loses the source URL. `CodeSource.location` (slot 0 / the
    // `location` field) must carry the load URL itself вЂ” `getCodeSource()
    // .getLocation()` callers and our tests read it as the code base URL,
    // not a derived path.
    let location_url = code_base.clone();

    // Nested JAR `code_source` is `jar:file:/outer.jar!/inner.jar`. Spring's
    // `Archive.create(File)` requires the outer filesystem path only.
    let code_base = if let Some(rest) = code_base.strip_prefix("jar:file:") {
        let outer = rest.split('!').next().unwrap_or(rest);
        let outer = outer.trim_start_matches('/');
        format!("file:/{}", outer)
    } else {
        code_base
    };

    // HotSpot-style path inside the `file:` URL (always `/C:/вЂ¦` on Windows).
    let raw_path = code_base
        .strip_prefix("file:")
        .map(str::to_string)
        .unwrap_or_else(|| code_base.clone());
    let fwd = raw_path.replace('\\', "/");
    let path = if fwd.len() >= 2 && fwd.as_bytes()[1] == b':' {
        format!("/{fwd}")
    } else {
        fwd
    };
    // A non-boot image module's `jrt:/<module>` (`image_module_code_base`;
    // interpreter round i1 wave 50, lane L5) keeps its own scheme.
    let (protocol, path) = match location_url.strip_prefix("jrt:") {
        Some(module_path) => ("jrt", module_path.to_string()),
        None => ("file", path),
    };

    // Build CodeSource(url=location, certs=[]) вЂ” signer certs are attached
    // as raw byte[] blocks so the reflective surface survives
    // `getCodeSource().getCertificates()` without requiring a full
    // `java.security.cert.Certificate` implementation.
    //
    // `java.net.URL` uses a 13-field JDK layout where slot 5 is `authority`.
    // Do **not** call `url_parse` here: our 6-field `URL_FIELD_FULL` index
    // collides with `authority`, corrupting `URL.toString()` / `toURI()` for
    // Spring Boot's launcher.
    //
    // GC-SAFETY: this whole tail allocates a long chain of objects
    // (url_obj -> path_obj/proto_obj/host_obj -> cs -> location_str ->
    // cert arrays -> pd) and reuses several of them well after later
    // allocations in the same chain -- pin each as it's produced and
    // re-read immediately before every subsequent use.
    // gen r4w3/rooting: `args[0]` is read again (by `Class.getClassLoader`)
    // after the <clinit>s below and is never refreshed; pin `this` first and
    // hand that native a fresh slice. Released with `url_pin` at the end.
    let this_pin = ctx.pin_native_root(this);
    // `--real-jdk`: the JDK's own `URL(String)` builds the location, so it
    // carries `sun.net.www.protocol.file.Handler` like HotSpot's. The minted
    // form below leaves `handler` null.
    let location = format!("{protocol}:{path}");
    let url_obj = match crate::jboss_module_loader::jdk_only_constructed_url(ctx, &location) {
        Some(url_obj) => url_obj,
        None => {
            let url_obj = try_alloc_concurrent_synthetic(ctx, "java/net/URL", 13)?;
            let mint_pin = ctx.pin_native_root(url_obj);
            let path_obj = ctx.create_string(&path);
            let path_pin = ctx.pin_native_root(path_obj);
            let proto_obj = ctx.create_string_uninterned(protocol);
            let proto_pin = ctx.pin_native_root(proto_obj);
            let host_obj = ctx.create_string("");
            let url_obj = ctx.read_native_pin(mint_pin, url_obj);
            let path_obj = ctx.read_native_pin(path_pin, path_obj);
            let proto_obj = ctx.read_native_pin(proto_pin, proto_obj);
            ctx.set_field(url_obj, 0, Value::Object(Some(proto_obj)));
            ctx.set_field(url_obj, 1, Value::Object(Some(host_obj)));
            ctx.set_field(url_obj, 2, Value::Int(-1));
            ctx.set_field(url_obj, 3, Value::Object(Some(path_obj)));
            ctx.set_field(url_obj, 5, Value::Object(None));
            ctx.set_field(url_obj, 6, Value::Object(Some(path_obj)));
            crate::jboss_module_loader::url_write_declared_field_defaults(&*ctx, url_obj);
            url_obj
        }
    };
    let url_pin = ctx.pin_native_root(url_obj);
    let cs_cid = ctx
        .ensure_class_initialized("java/security/CodeSource")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let cs_num_fields = ctx.class_num_total_fields(cs_cid).max(2);
    let cs = ctx.alloc_object(cs_cid, cs_num_fields);
    let cs_pin = ctx.pin_native_root(cs);
    let url_obj = ctx.read_native_pin(url_pin, url_obj);
    // Slot-based writes cover the synthetic layout; name-based writes
    // cover the real-JDK-loaded layout. At least one lands on the right
    // slot for each mode.
    //
    // FIX: The synthetic `CodeSource` layout exposes the load URL as a
    // String in slot 0 (this is what `getCodeSource().getLocation()` style
    // callers and the T19.N1 test read). Populate it with the real code base
    // URL (`location_url`) instead of leaving it empty / a bare URL object.
    let location_str = ctx.create_string(&location_url);
    let cs = ctx.read_native_pin(cs_pin, cs);
    let url_obj = ctx.read_native_pin(url_pin, url_obj);
    ctx.set_field(cs, 0, Value::Object(Some(location_str)));
    // Real-JDK `CodeSource.location` is typed `java.net.URL`; the name-based
    // write targets that layout with the constructed URL object.
    ctx.set_field_by_name(cs, "location", Value::Object(Some(url_obj)));

    // Attach signer certs as fresh byte[] copies on slot 1.
    if !certs.is_empty() {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, certs.len());
        let arr_pin = ctx.pin_native_root(arr);
        for (i, cert) in certs.iter().enumerate() {
            let cert_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, cert.len());
            for (j, &b) in cert.iter().enumerate() {
                ctx.set_array_element(cert_arr, j, Value::Int(b as i8 as i32));
            }
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(cert_arr)));
        }
        let arr = ctx.read_native_pin(arr_pin, arr);
        let cs = ctx.read_native_pin(cs_pin, cs);
        // BY NAME only: raw slot 1 is `signers` on a real `CodeSource`, so the
        // raw write parked a `Certificate[]`-shaped array in the signer field
        // and the by-name write beside it put the real one in `certs`. The
        // by-name write covers both layouts on its own.
        ctx.set_field_by_name(cs, "certs", Value::Object(Some(arr)));
    }
    let cs = ctx.read_native_pin(cs_pin, cs);

    // Build ProtectionDomain(codesource=cs, permissions=<empty>,
    // classloader=<defining loader>, principals=empty).  SBR-13: populate the
    // real defining ClassLoader and a non-null (empty) Permissions collection
    // so `getClassLoader()` / `getPermissions()` match HotSpot's shape instead
    // of returning null / a placeholder.
    let pd_cid = ctx
        .ensure_class_initialized("java/security/ProtectionDomain")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let pd_num_fields = ctx.class_num_total_fields(pd_cid).max(4);
    let pd = ctx.alloc_object(pd_cid, pd_num_fields);
    let pd_pin = ctx.pin_native_root(pd);
    let cs = ctx.read_native_pin(cs_pin, cs);
    // Resolve the class's defining loader exactly like `Class.getClassLoader()`
    // (bootstrap в†’ null, app classpath в†’ the singleton AppClassLoader).
    let this = ctx.read_native_pin(this_pin, this);
    let classloader = if loader_of_mirror {
        native_class_get_class_loader(ctx, &[Value::Object(Some(this))])
            .ok()
            .flatten()
            .unwrap_or(Value::Object(None))
    } else {
        Value::Object(Some(this))
    };
    let pd = ctx.read_native_pin(pd_pin, pd);
    let cs = ctx.read_native_pin(cs_pin, cs);
    // The unpin used to be HERE, one statement too early.
    //
    // `url_pin` is below `pd_pin`, so releasing it releases `pd`'s pin as well
    // — and `populate_protection_domain_fields` allocates. A moving young
    // collection inside it therefore relocated `pd` with nothing rooting it,
    // and this function returned the pre-move address.
    //
    // The interpreter's return-value barrier cannot recover that:
    // `safe_native_call` heals a stale return through `load_and_forward`, which
    // reads the forwarding marker at the old address, and that marker is gone
    // once the allocator has re-served the span. MEASURED on BindableTests
    // under `CRATONVM_DBG_GC_STRESS=262144`: `[deadref-nret]` named this native
    // returning an address in the emptied semispace, reached from ByteBuddy's
    // `trySelfResolve(Class)` and `JavaDispatcher$DynamicClassLoader.invoker()`.
    let populated =
        populate_protection_domain_fields(ctx, pd, Value::Object(Some(cs)), classloader);
    // Read `pd` back through its pin BEFORE the unpin, then release the whole
    // group at once.
    let pd = ctx.read_native_pin(pd_pin, pd);
    // gen r4w3/rooting: `this_pin` (below `url_pin`) is the lowest pin of the
    // group now; releasing it releases the whole group.
    ctx.unpin_native_roots(this_pin);
    populated?;

    Ok(pd)
}

/// `java/lang/Class.getSigners()[Ljava/lang/Object;`
///
/// Returns the signer certificates of this class as an `Object[]` whose
/// elements are `byte[]` copies of the raw PKCS#7 signer blocks from the
/// class's `CodeSource.certificates`.  Returns null for unsigned classes
/// (including every bootstrap class, since jimage classes have no signers).
///
/// Real JDK returns `Certificate[]`; we approximate with `Object[]` + `byte[]`
/// elements because our synthetic JDK surface does not fully implement
/// `java.security.cert.Certificate`.  Callers that merely check
/// `signers != null` or use the bytes for digest-match work correctly;
/// callers that cast the elements to `Certificate` in synthetic mode will
/// see `ClassCastException`, which is the correct failure mode for
/// a class not yet wired up.
pub fn native_class_get_signers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    let certs = ctx.class_code_source_certs(class_id);
    if certs.is_empty() {
        return Ok(Some(Value::Object(None)));
    }

    // gc-common w20-f: the outer array is rooted across each certificate's
    // allocation (every element was stored into its first address).
    let mut scope = NativeHandleScope::new(ctx);
    let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, certs.len());
    let arr_h = scope.root(arr);
    for (i, cert) in certs.iter().enumerate() {
        // Fresh byte[] copy вЂ” never leak the internal `CodeSource.certificates`
        // Vec<Vec<u8>> pointer to the caller.
        let cert_arr = scope.new_array(cratonvm_types::ArrayElementType::Byte, cert.len());
        for (j, &b) in cert.iter().enumerate() {
            scope.set_array_element(cert_arr, j, Value::Int(b as i8 as i32));
        }
        let arr: ObjectRef = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(cert_arr)));
    }
    let arr: ObjectRef = scope.get(&arr_h);
    Ok(Some(Value::Object(Some(arr))))
}

/// `java/lang/Class.setSigners([Ljava/lang/Object;)V`
///
/// Documented no-op.  Real HotSpot stores the passed signers on the
/// `Class` metadata where `getSigners()` later returns them.  We do not
/// currently track mutable per-class signers (the `CodeSource` installed
/// at class-define time is immutable in our class store), so this setter
/// silently discards its argument.
///
/// A `tracing::debug!` fires when `setSigners` is called so any runtime
/// reliance on the post-define-time setter surfaces in logs rather than
/// silently corrupting security policy enforcement.
///
/// Per the JDK spec `setSigners` is effectively package-private to
/// `java.lang.ClassLoader`; arbitrary user code calling it is either a
/// test harness or a misbehaving agent.  We do not enforce a caller-class
/// check because that would require walking the stack for every call
/// (this native is rarely invoked after class loading completes).
pub fn native_class_set_signers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = mirror_class_name(ctx, this).unwrap_or_else(|| "<unknown>".to_string());
    let signer_count = match args.get(1) {
        Some(Value::Object(Some(arr))) => ctx.array_length(*arr),
        _ => 0,
    };
    tracing::debug!(
        target: "cratonvm_native_builtins::lang_class",
        class = %class_name,
        signer_count = signer_count,
        "Class.setSigners called вЂ” documented no-op (signers not mutable post-define)"
    );
    Ok(None)
}

