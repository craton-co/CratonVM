// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.lang.reflect` / `java.lang.invoke` natives: VarHandle, MethodHandles, Module, Package, StackWalker, constant/switch bootstraps.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;
use cratonvm_classloading::module::ALL_UNNAMED_TARGET;

// ---------------------------------------------------------------------------
// java.lang.reflect extras: Parameter, Executable
// ---------------------------------------------------------------------------
pub(crate) fn register_phase55_reflect(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- java.lang.reflect.Parameter (3-field: name=0, modifiers=1, type=2) ---
    let param = "java/lang/reflect/Parameter";
    r.register(param, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(param, "getModifiers", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(param, "getType", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    // `isNamePresent` is deliberately NOT registered here. The constant `true`
    // that used to occupy this slot claimed a real parameter name even for the
    // JDK's synthesized `arg0`/`arg1` placeholders — exactly the predicate
    // Spring's `StandardReflectionParameterNameDiscoverer` branches on.
    // `lang_reflect::native_parameter_is_name_present` (installed earlier, in
    // BOTH run modes, via `register_annotation_overrides` ->
    // `register_wp2_1_natives`) answers it correctly from the stored name.
    r.register(param, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    // --- java.lang.reflect.Modifier (static utility) ---
    let modifier = "java/lang/reflect/Modifier";
    r.register(modifier, "isPublic", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0001 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isPrivate", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0002 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isProtected", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0004 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isStatic", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0008 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isFinal", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0010 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isSynchronized", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0020 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isVolatile", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0040 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isTransient", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0080 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isNative", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0100 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isAbstract", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0400 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isInterface", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0200 != 0 { 1 } else { 0 })))
    });
    r.register(modifier, "isStrict", "(I)Z", |_ctx, args| {
        let flags = args[0].as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 0x0800 != 0 { 1 } else { 0 })))
    });
    r.register(
        modifier,
        "toString",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            let flags = args[0].as_int().unwrap_or(0);
            let mut parts = Vec::new();
            if flags & 0x0001 != 0 {
                parts.push("public");
            }
            if flags & 0x0002 != 0 {
                parts.push("private");
            }
            if flags & 0x0004 != 0 {
                parts.push("protected");
            }
            if flags & 0x0008 != 0 {
                parts.push("static");
            }
            if flags & 0x0010 != 0 {
                parts.push("final");
            }
            if flags & 0x0020 != 0 {
                parts.push("synchronized");
            }
            if flags & 0x0040 != 0 {
                parts.push("volatile");
            }
            if flags & 0x0080 != 0 {
                parts.push("transient");
            }
            if flags & 0x0100 != 0 {
                parts.push("native");
            }
            if flags & 0x0200 != 0 {
                parts.push("interface");
            }
            if flags & 0x0400 != 0 {
                parts.push("abstract");
            }
            let s = ctx.create_string(&parts.join(" "));
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // --- Proxy --- (real implementation in lib.rs::register_reflect_proxy_natives)
    r.set_category(__prev_cat);
}

// =============================================================================
// VarHandle expansion — get/set/CAS with object+field semantics
// VarHandle = 2-field synthetic (target=0, fieldIndex=1 Int)
// =============================================================================

// VarHandle internal field layout (3 slots):
//   slot 0: class_id as Int (for static VarHandles) OR target ObjectRef (for instance VH,
//           not yet set at factory time — set at get/set time via the receiver)
//   slot 1: field_index as Int
//   slot 2: kind as Int (0 = instance field, 1 = static field, 2 = array element)
pub(crate) const VH_CLASS_OR_TARGET: usize = 0;

pub(crate) const VH_FIELD_INDEX: usize = 1;

pub(crate) const VH_IS_STATIC: usize = 2;

pub(crate) const VH_NUM_FIELDS: usize = 3;

pub(crate) const VH_KIND_ARRAY: i32 = 2;

pub(crate) const VH_KIND_MEMORY_SEGMENT: i32 = 3;

pub(crate) const VH_KIND_BYTE_ARRAY_VIEW_LE: i32 = 4;

pub(crate) const VH_KIND_BYTE_ARRAY_VIEW_BE: i32 = 5;

// --- W6-1: the two describe-yourself slots -------------------------------
//
// `VarHandle.varType()` and `VarHandle.coordinateTypes()` are CONCRETE JDK
// bytecode that reads `this.vform` (slot 0 of the real class) and walks a
// `VarForm`/`MethodType` chain CratonVM never populates — on a CratonVM
// handle slot 0 holds an `Int` ClassId, so the real body cannot run at all.
// Answering them needs the two `Class` mirrors the factory was handed, and
// nothing in the 3-slot layout above records a `Class` at all.
//
// They are stored PAST the real `java.lang.invoke.VarHandle` layout, which
// declares exactly four instance fields in JDK 25 (`vform`, `exact`,
// `methodTypeTable`, `methodHandleTable` — `javap -p`), so unlike slots 0-2
// they alias no real-JDK field: `alloc_concurrent_synthetic` sizes the object
// `num_fields.max(class_num_total_fields)`, and asking for
// `VH_META_NUM_FIELDS` grows it without moving anything.
//
// WHICH slots is NOT this file's decision. `lang_invoke.rs` imposes its own
// six-slot meaning on the same `java/lang/invoke/VarHandle` object, and W6-1
// picked 4 and 5 — which are `VH_FIELD_INDEX` (Int) and `VH_CLASS_ID` (Int)
// over there, for the same "past the real fields" reason. Two files, two
// meanings, one object. The map now lives in ONE place; see the SHARED SLOT
// MAP block in `native-builtins/src/lang_invoke.rs` next to `VH_FIELD_COUNT`,
// and add nothing here without adding it there.
//
// Every read is guarded by `object_num_fields(vh) > VH_COORD0` AND by slot
// `VH_VAR_TYPE` actually holding an object, so a VarHandle minted by a
// factory that does NOT stamp them (`phases_late/foreign_ffm.rs`'s
// memory-segment handles, which are not this lane's files, the legacy
// sub-`VH_NUM_FIELDS` array convention, and every `lang_invoke.rs` factory,
// which allocates only `VH_FIELD_COUNT` slots) reads as "no metadata" and the
// accessors REFUSE rather than invent a plausible `Class`.
//
// Defined as ALIASES rather than a `use` re-export so this module's public
// surface is byte-identical to what it was (`phases_late.rs` does
// `pub use reflect_invoke::*` and `lib.rs` does `use lang_invoke::*`; two
// globs exporting one name is an ambiguity waiting for its first user).
/// Slot [`crate::lang_invoke::VH_META_VAR_TYPE`]: the `Class` mirror of the
/// variable the handle accesses — exactly the object `varType()` must return.
pub(crate) const VH_VAR_TYPE: usize = crate::lang_invoke::VH_META_VAR_TYPE;

/// Slot [`crate::lang_invoke::VH_META_COORD0`]: the `Class` mirror of the
/// handle's LEADING coordinate — the receiver class for an instance-field
/// handle, the array class for an array-element handle, and `null` for a
/// static-field handle (which has no coordinates at all). Trailing coordinates
/// are implied by the kind tag and are rebuilt in [`vh_coordinate_mirrors`]
/// rather than stored.
pub(crate) const VH_COORD0: usize = crate::lang_invoke::VH_META_COORD0;

/// Allocation size for a VarHandle that carries the describe-yourself slots.
pub(crate) const VH_META_NUM_FIELDS: usize = crate::lang_invoke::VH_META_SLOT_COUNT;

/// Resolve `(declaring class, ABSOLUTE heap slot)` for a named instance field,
/// walking the inheritance chain. Returns `None` if not found.
///
/// The index is [`FieldMetadata::slot_index`], which the VM documents as the
/// absolute heap field index and computes as `first_field_index + n`th
/// instance field — i.e. exactly what `get_field`/`set_field` take. This used
/// to return the `enumerate()` position over `declared_fields`, which counts
/// STATIC fields too and ignores inherited ones, so it was only accidentally
/// correct for a class that declares no statics before the field and inherits
/// none (`RJdkHandles$Holder.i/l/s` are such fields; `Holder.arr`, declared
/// after `static int stat`, was not).
pub(crate) fn vh_find_instance_field(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> Option<(ClassId, usize)> {
    let mut current = class_id;
    loop {
        for f in ctx.declared_fields(current) {
            if f.name == name && !f.is_static {
                return Some((current, f.slot_index));
            }
        }
        current = ctx.superclass_of(current)?;
    }
}

/// The JVM field descriptor of a named instance field, walking the chain.
/// `None` when no such instance field exists.
pub(crate) fn vh_instance_field_descriptor(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> Option<String> {
    let mut current = class_id;
    loop {
        for f in ctx.declared_fields(current) {
            if f.name == name && !f.is_static {
                return Some(f.descriptor);
            }
        }
        current = ctx.superclass_of(current)?;
    }
}

/// `(static-block index, descriptor)` of a named static field.
///
/// The index is [`FieldMetadata::slot_index`], which for a static field is
/// its position among the class's STATIC fields only — the indexing
/// `get_static_field`/`set_static_field` document and `static_field_index_by_name`
/// computes. A position over all declared fields (statics *and* instances)
/// is a different number the moment the class declares any instance field
/// first, which is the ordinary case.
pub(crate) fn vh_find_static_field(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> Option<(usize, String)> {
    ctx.declared_fields(class_id)
        .into_iter()
        .find(|f| f.is_static && f.name == name)
        .map(|f| (f.slot_index, f.descriptor))
}

/// Can the VM enumerate any fields for this class (or a superclass)?
///
/// This is the guard that separates "the field genuinely does not exist", where
/// `findVarHandle` MUST raise `NoSuchFieldException` exactly as the JDK does,
/// from "we cannot see this class's fields at all" (an unresolved mirror, a
/// mock `NativeContext` in a unit test), where raising would turn a modelling
/// gap into a spurious failure. Only the first case throws.
pub(crate) fn vh_class_fields_visible(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    let mut current = class_id;
    loop {
        if !ctx.declared_fields(current).is_empty() {
            return true;
        }
        match ctx.superclass_of(current) {
            Some(parent) if parent != current => current = parent,
            _ => return false,
        }
    }
}

/// Read the value a VarHandle points to (instance or static).
/// Falls back to array-element 0 when `vh` has fewer than `VH_NUM_FIELDS` object slots
/// (i.e., the "VarHandle" is itself an array — a legacy calling convention used in some tests).
pub(crate) fn vh_get(ctx: &dyn NativeContext, vh: ObjectRef, target: Option<ObjectRef>) -> Value {
    // Legacy fallback: if the object is backed by < VH_NUM_FIELDS slots, treat it as an array.
    if ctx.object_num_fields(vh) < VH_NUM_FIELDS {
        return ctx.get_array_element(vh, 0);
    }
    let is_static = ctx.get_field(vh, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
    let field_idx = ctx.get_field(vh, VH_FIELD_INDEX).as_int().unwrap_or(0) as usize;
    if is_static {
        let class_id_raw = ctx.get_field(vh, VH_CLASS_OR_TARGET).as_int().unwrap_or(0) as u32;
        ctx.get_static_field(ClassId::new(class_id_raw), field_idx)
    } else {
        let obj = match target.or_else(|| {
            if let Value::Object(o) = ctx.get_field(vh, VH_CLASS_OR_TARGET) {
                o
            } else {
                None
            }
        }) {
            Some(o) => o,
            None => return Value::Object(None),
        };
        ctx.get_field(obj, field_idx)
    }
}

/// Resolve the `(holder object, field slot)` a non-static VarHandle points at,
/// so a caller can use the real `compare_and_swap_field` primitive instead of
/// a `vh_get` / `vh_set` pair with a race between them.
///
/// Returns `None` for static VarHandles (no static-field CAS primitive exists)
/// and for the legacy array-backed convention that `vh_get` falls back to
/// (`object_num_fields(vh) < VH_NUM_FIELDS`), both of which keep the old path.
pub(crate) fn vh_instance_slot(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
    target: Option<ObjectRef>,
) -> Option<(ObjectRef, usize)> {
    if ctx.object_num_fields(vh) < VH_NUM_FIELDS {
        return None;
    }
    if ctx.get_field(vh, VH_IS_STATIC).as_int().unwrap_or(0) != 0 {
        return None;
    }
    let field_idx = ctx.get_field(vh, VH_FIELD_INDEX).as_int().unwrap_or(0) as usize;
    let obj = target.or_else(|| {
        if let Value::Object(o) = ctx.get_field(vh, VH_CLASS_OR_TARGET) {
            o
        } else {
            None
        }
    })?;
    Some((obj, field_idx))
}

/// Write the value a VarHandle points to (instance or static).
pub(crate) fn vh_set(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    target: Option<ObjectRef>,
    value: Value,
) {
    // Legacy fallback: if the object has < VH_NUM_FIELDS slots, treat it as an array.
    if ctx.object_num_fields(vh) < VH_NUM_FIELDS {
        ctx.set_array_element(vh, 0, value);
        return;
    }
    let is_static = ctx.get_field(vh, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
    let field_idx = ctx.get_field(vh, VH_FIELD_INDEX).as_int().unwrap_or(0) as usize;
    if is_static {
        let class_id_raw = ctx.get_field(vh, VH_CLASS_OR_TARGET).as_int().unwrap_or(0) as u32;
        ctx.set_static_field(ClassId::new(class_id_raw), field_idx, value);
    } else if let Some(obj) = target.or_else(|| {
        if let Value::Object(o) = ctx.get_field(vh, VH_CLASS_OR_TARGET) {
            o
        } else {
            None
        }
    }) {
        ctx.set_field(obj, field_idx, value);
    }
}

/// Read the value a VarHandle points to using volatile (atomic) access for instance fields.
pub(crate) fn vh_get_volatile(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
    target: Option<ObjectRef>,
) -> Value {
    if ctx.object_num_fields(vh) < VH_NUM_FIELDS {
        return ctx.get_array_element(vh, 0);
    }
    let is_static = ctx.get_field(vh, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
    let field_idx = ctx.get_field(vh, VH_FIELD_INDEX).as_int().unwrap_or(0) as usize;
    if is_static {
        let class_id_raw = ctx.get_field(vh, VH_CLASS_OR_TARGET).as_int().unwrap_or(0) as u32;
        ctx.get_static_field(ClassId::new(class_id_raw), field_idx)
    } else {
        let obj = match target.or_else(|| {
            if let Value::Object(o) = ctx.get_field(vh, VH_CLASS_OR_TARGET) {
                o
            } else {
                None
            }
        }) {
            Some(o) => o,
            None => return Value::Object(None),
        };
        ctx.get_field_volatile(obj, field_idx)
    }
}

/// Write the value a VarHandle points to using volatile (atomic) access for instance fields.
pub(crate) fn vh_set_volatile(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    target: Option<ObjectRef>,
    value: Value,
) {
    if ctx.object_num_fields(vh) < VH_NUM_FIELDS {
        ctx.set_array_element(vh, 0, value);
        return;
    }
    let is_static = ctx.get_field(vh, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
    let field_idx = ctx.get_field(vh, VH_FIELD_INDEX).as_int().unwrap_or(0) as usize;
    if is_static {
        let class_id_raw = ctx.get_field(vh, VH_CLASS_OR_TARGET).as_int().unwrap_or(0) as u32;
        ctx.set_static_field(ClassId::new(class_id_raw), field_idx, value);
    } else if let Some(obj) = target.or_else(|| {
        if let Value::Object(o) = ctx.get_field(vh, VH_CLASS_OR_TARGET) {
            o
        } else {
            None
        }
    }) {
        ctx.set_field_volatile(obj, field_idx, value);
    }
}

/// Read the kind tag from a VarHandle (0=instance, 1=static, 2=array).
pub(crate) fn vh_kind(ctx: &dyn NativeContext, vh: ObjectRef) -> i32 {
    if crate::lang_invoke::p67_memory_segment_var_handle_width(ctx, vh).is_some() {
        return VH_KIND_MEMORY_SEGMENT;
    }
    if ctx.object_num_fields(vh) < VH_NUM_FIELDS {
        return 0;
    }
    ctx.get_field(vh, VH_IS_STATIC).as_int().unwrap_or(0)
}

/// For an array-element VarHandle invocation, extract `(array_ref, index)`
/// from the polymorphic call args.  Layout: `[vh, array_obj, idx_int, ...]`.
pub(crate) fn vh_array_target(args: &[Value]) -> Option<(ObjectRef, usize)> {
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return None,
    };
    let idx = match args.get(2) {
        Some(Value::Int(i)) => *i as usize,
        Some(Value::Long(i)) => *i as usize,
        _ => return None,
    };
    Some((arr, idx))
}

/// Bounds-check an array-element VarHandle access BEFORE it happens.
///
/// `NativeContext::get_array_element` fails **safe**: an out-of-range index
/// reads back `Int(0)`, and `set_array_element` drops the write. So
/// `MethodHandles.arrayElementVarHandle(int[].class).get(arr, 7)` on a
/// three-element array answered `0` — a value indistinguishable from a real
/// element — where the JDK raises `ArrayIndexOutOfBoundsException` (a
/// VarHandle array accessor bounds-checks exactly as `iaload` does). That is
/// the fabricated-success shape: the access "succeeds" and the caller has no
/// way to tell it read past the end.
///
/// A negative index arrives here as a very large `usize` (`vh_array_target`
/// casts) and is caught by the same comparison; the message reports the index
/// the caller actually passed, not the cast one.
fn vh_array_bounds_check(ctx: &dyn NativeContext, args: &[Value]) -> Result<(), MethodCallFailed> {
    let Some((arr, idx)) = vh_array_target(args) else {
        return Ok(());
    };
    let len = ctx.array_length(arr);
    if idx >= len {
        let reported = match args.get(2) {
            Some(Value::Int(i)) => *i,
            Some(Value::Long(i)) => *i as i32,
            _ => idx as i32,
        };
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: reported,
            message: Some(format!("Index {reported} out of bounds for length {len}")),
        }
        .into());
    }
    Ok(())
}

/// Read element from an array-kind VarHandle call: `args = [vh, array, idx]`.
pub(crate) fn vh_array_get(ctx: &dyn NativeContext, args: &[Value]) -> Value {
    match vh_array_target(args) {
        Some((arr, idx)) => ctx.get_array_element(arr, idx),
        None => Value::Object(None),
    }
}

/// Write the element for an array-kind VarHandle call.
/// `args = [vh, array, idx, value]` — `value_arg_index` is `3`.
pub(crate) fn vh_array_set(ctx: &mut dyn NativeContext, args: &[Value], value_arg_index: usize) {
    if let Some((arr, idx)) = vh_array_target(args) {
        let val = args
            .get(value_arg_index)
            .copied()
            .unwrap_or(Value::Object(None));
        ctx.set_array_element(arr, idx, val);
    }
}

pub(crate) fn vh_byte_array_view_width(elem: u8) -> usize {
    match elem {
        b'J' | b'D' => 8,
        b'I' | b'F' => 4,
        b'S' | b'C' => 2,
        _ => 1,
    }
}

pub(crate) fn vh_byte_array_view_elem_from_mirror(
    ctx: &dyn NativeContext,
    arg: Option<&Value>,
) -> u8 {
    let Some(Value::Object(Some(mirror))) = arg else {
        return b'J';
    };
    let name = crate::lang_class::mirror_class_name(ctx, *mirror).unwrap_or_default();
    if let Some(desc) = name.strip_prefix('[') {
        return desc.as_bytes().first().copied().unwrap_or(b'J');
    }
    match name.as_str() {
        "long" | "java/lang/Long" => b'J',
        "int" | "java/lang/Integer" => b'I',
        "short" | "java/lang/Short" => b'S',
        "char" | "java/lang/Character" => b'C',
        "float" | "java/lang/Float" => b'F',
        "double" | "java/lang/Double" => b'D',
        "byte" | "java/lang/Byte" => b'B',
        _ => b'J',
    }
}

pub(crate) fn vh_byte_order_is_little(ctx: &dyn NativeContext, arg: Option<&Value>) -> bool {
    let Some(Value::Object(Some(order))) = arg else {
        return true;
    };
    if let Value::Object(Some(name_obj)) = ctx.get_field_by_name(*order, "name") {
        if let Some(name) = ctx.read_string(name_obj) {
            if name.contains("LITTLE") {
                return true;
            }
            if name.contains("BIG") {
                return false;
            }
        }
    }
    ctx.get_field(*order, 0)
        .as_int()
        .map(|v| v != 0)
        .unwrap_or(true)
}

pub(crate) fn vh_byte_array_view_target(args: &[Value]) -> Option<(ObjectRef, usize)> {
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return None,
    };
    let idx = match args.get(2) {
        Some(Value::Int(i)) if *i >= 0 => *i as usize,
        Some(Value::Long(i)) if *i >= 0 => *i as usize,
        _ => return None,
    };
    Some((arr, idx))
}

pub(crate) fn vh_byte_array_view_meta(ctx: &dyn NativeContext, vh: ObjectRef) -> (u8, bool) {
    let elem = ctx
        .get_field(vh, VH_CLASS_OR_TARGET)
        .as_int()
        .map(|v| v as u8)
        .unwrap_or(b'J');
    let le = vh_kind(ctx, vh) != VH_KIND_BYTE_ARRAY_VIEW_BE;
    (elem, le)
}

pub(crate) fn vh_byte_array_view_get(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
) -> Value {
    let Some((arr, idx)) = vh_byte_array_view_target(args) else {
        return Value::Object(None);
    };
    let (elem, le) = vh_byte_array_view_meta(ctx, vh);
    let width = vh_byte_array_view_width(elem);
    let mut raw = 0u64;
    for i in 0..width {
        let b = ctx.get_array_element(arr, idx + i).as_int().unwrap_or(0) as u8 as u64;
        if le {
            raw |= b << (8 * i);
        } else {
            raw = (raw << 8) | b;
        }
    }
    match elem {
        b'J' => Value::Long(raw as i64),
        b'D' => Value::Double(f64::from_bits(raw)),
        b'I' => Value::Int(raw as u32 as i32),
        b'F' => Value::Float(f32::from_bits(raw as u32)),
        b'S' => Value::Int(raw as u16 as i16 as i32),
        b'C' => Value::Int(raw as u16 as i32),
        _ => Value::Int(raw as u8 as i8 as i32),
    }
}

pub(crate) fn vh_byte_array_view_set(
    ctx: &mut dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
    value_arg_index: usize,
) {
    let Some((arr, idx)) = vh_byte_array_view_target(args) else {
        return;
    };
    let (elem, le) = vh_byte_array_view_meta(ctx, vh);
    let width = vh_byte_array_view_width(elem);
    let value = args.get(value_arg_index).copied().unwrap_or(Value::Int(0));
    let raw = match value {
        Value::Long(v) => v as u64,
        Value::Double(v) => v.to_bits(),
        Value::Int(v) => v as u32 as u64,
        Value::Float(v) => v.to_bits() as u64,
        _ => 0,
    };
    for i in 0..width {
        let shift = if le { 8 * i } else { 8 * (width - 1 - i) };
        let b = ((raw >> shift) & 0xff) as u8 as i8 as i32;
        ctx.set_array_element(arr, idx + i, Value::Int(b));
    }
}

pub(crate) fn vh_memory_segment_target(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Option<(i64, i64, i64, i64)> {
    let seg = match args.get(1) {
        Some(Value::Object(Some(seg))) => *seg,
        _ => return None,
    };
    if ctx.object_num_fields(seg) < 6 {
        return None;
    }
    let offset = match args.get(2) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let ptr = match ctx.get_field(seg, 0) {
        Value::Long(v) => v,
        _ => return None,
    };
    let size = match ctx.get_field(seg, 1) {
        Value::Long(v) => v,
        _ => return None,
    };
    let base_offset = match ctx.get_field(seg, 5) {
        Value::Long(v) => v,
        _ => 0,
    };
    Some((ptr, size, base_offset, offset))
}

pub(crate) fn vh_memory_segment_get(
    ctx: &dyn NativeContext,
    vh: ObjectRef,
    args: &[Value],
) -> Value {
    let width = crate::lang_invoke::p67_memory_segment_var_handle_width(ctx, vh)
        .or_else(|| ctx.get_field(vh, VH_FIELD_INDEX).as_int())
        .unwrap_or(1)
        .clamp(1, 8) as i64;
    let little_endian = ctx
        .get_field(vh, VH_CLASS_OR_TARGET)
        .as_int()
        .map(|v| v != 0)
        .unwrap_or(false);
    let Some((ptr, size, base_offset, offset)) = vh_memory_segment_target(ctx, args) else {
        return Value::Int(0);
    };
    if ptr == 0 || offset < 0 || offset.saturating_add(width) > size {
        return Value::Int(0);
    }
    let absolute_offset = base_offset.saturating_add(offset);
    let addr = (ptr as usize).wrapping_add(absolute_offset as usize) as *const u8;
    unsafe {
        match width {
            1 => Value::Int(*(addr as *const i8) as i32),
            2 => {
                let bytes = [*addr, *addr.add(1)];
                let v = if little_endian {
                    i16::from_le_bytes(bytes)
                } else {
                    i16::from_be_bytes(bytes)
                };
                Value::Int(v as i32)
            }
            4 => {
                let bytes = [*addr, *addr.add(1), *addr.add(2), *addr.add(3)];
                let v = if little_endian {
                    i32::from_le_bytes(bytes)
                } else {
                    i32::from_be_bytes(bytes)
                };
                Value::Int(v)
            }
            8 => {
                let bytes = [
                    *addr,
                    *addr.add(1),
                    *addr.add(2),
                    *addr.add(3),
                    *addr.add(4),
                    *addr.add(5),
                    *addr.add(6),
                    *addr.add(7),
                ];
                let v = if little_endian {
                    i64::from_le_bytes(bytes)
                } else {
                    i64::from_be_bytes(bytes)
                };
                Value::Long(v)
            }
            _ => Value::Int(0),
        }
    }
}

pub(crate) fn vh_memory_segment_set(ctx: &dyn NativeContext, vh: ObjectRef, args: &[Value]) {
    let width = crate::lang_invoke::p67_memory_segment_var_handle_width(ctx, vh)
        .or_else(|| ctx.get_field(vh, VH_FIELD_INDEX).as_int())
        .unwrap_or(1)
        .clamp(1, 8) as i64;
    let little_endian = ctx
        .get_field(vh, VH_CLASS_OR_TARGET)
        .as_int()
        .map(|v| v != 0)
        .unwrap_or(false);
    let Some((ptr, size, base_offset, offset)) = vh_memory_segment_target(ctx, args) else {
        return;
    };
    if ptr == 0 || offset < 0 || offset.saturating_add(width) > size {
        return;
    }
    let value = args.get(3).copied().unwrap_or(Value::Int(0));
    let absolute_offset = base_offset.saturating_add(offset);
    let addr = (ptr as usize).wrapping_add(absolute_offset as usize) as *mut u8;
    unsafe {
        match (width, value) {
            (1, Value::Int(v)) => {
                *addr = v as u8;
            }
            (2, Value::Int(v)) => {
                let bytes = if little_endian {
                    (v as i16).to_le_bytes()
                } else {
                    (v as i16).to_be_bytes()
                };
                addr.copy_from_nonoverlapping(bytes.as_ptr(), 2);
            }
            (4, Value::Int(v)) => {
                let bytes = if little_endian {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                };
                addr.copy_from_nonoverlapping(bytes.as_ptr(), 4);
            }
            (8, Value::Long(v)) => {
                let bytes = if little_endian {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                };
                addr.copy_from_nonoverlapping(bytes.as_ptr(), 8);
            }
            _ => {}
        }
    }
}

/// Auto-box a primitive Value into its wrapper object for signature-polymorphic
/// VarHandle returns. When the call-site expects Ljava/lang/Object;, primitives
/// must be wrapped (e.g. Int(42) → Integer object with field 0 = Int(42)).
pub(crate) fn vh_auto_box(ctx: &mut dyn NativeContext, val: Value) -> Result<Value, MethodCallFailed> {
    match val {
        Value::Int(_) => {
            let wrapper = crate::try_alloc_concurrent_synthetic(ctx, "java/lang/Integer", 1)?;
            ctx.set_field(wrapper, 0, val);
            Ok(Value::Object(Some(wrapper)))
        }
        Value::Long(_) => {
            let wrapper = crate::try_alloc_concurrent_synthetic(ctx, "java/lang/Long", 1)?;
            ctx.set_field(wrapper, 0, val);
            Ok(Value::Object(Some(wrapper)))
        }
        Value::Float(_) => {
            let wrapper = crate::try_alloc_concurrent_synthetic(ctx, "java/lang/Float", 1)?;
            ctx.set_field(wrapper, 0, val);
            Ok(Value::Object(Some(wrapper)))
        }
        Value::Double(_) => {
            let wrapper = crate::try_alloc_concurrent_synthetic(ctx, "java/lang/Double", 1)?;
            ctx.set_field(wrapper, 0, val);
            Ok(Value::Object(Some(wrapper)))
        }
        _ => Ok(val), // Already an object reference or null
    }
}

// =============================================================================
// W6-1: `VarHandle.varType()` / `VarHandle.coordinateTypes()`
// =============================================================================

/// Stamp the describe-yourself slots on a freshly minted VarHandle.
///
/// `coord0` is `None` for a static-field handle, which the JDK specifies as
/// having zero coordinates.
fn vh_stamp_meta(
    ctx: &mut dyn NativeContext,
    vh_obj: ObjectRef,
    var_type: Option<ObjectRef>,
    coord0: Option<ObjectRef>,
) {
    if ctx.object_num_fields(vh_obj) <= VH_COORD0 {
        return;
    }
    ctx.set_field(vh_obj, VH_VAR_TYPE, Value::Object(var_type));
    ctx.set_field(vh_obj, VH_COORD0, Value::Object(coord0));
}

/// Read one describe-yourself slot. `None` means "this handle carries no
/// metadata" — never "the answer is null".
fn vh_meta_mirror(ctx: &dyn NativeContext, vh: ObjectRef, slot: usize) -> Option<ObjectRef> {
    if ctx.object_num_fields(vh) <= VH_COORD0 {
        return None;
    }
    match ctx.get_field(vh, slot) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// `varType()`'s answer, or `None` when this handle was not stamped.
///
/// Presence is keyed on `VH_VAR_TYPE` alone: every stamping factory writes a
/// non-null `Class` there, and no handle has a null variable type, so a null
/// slot is unambiguously "unstamped".
fn vh_var_type_mirror(ctx: &dyn NativeContext, vh: ObjectRef) -> Option<ObjectRef> {
    vh_meta_mirror(ctx, vh, VH_VAR_TYPE)
}

/// `coordinateTypes()`'s answer as a mirror list, or `None` when this handle
/// was not stamped.
///
/// The leading coordinate is stored; the trailing ones follow from the kind
/// tag and are rebuilt here:
///
/// | kind | coordinates |
/// | --- | --- |
/// | instance field (0) | `{receiverClass}` |
/// | static field (1) | `{}` |
/// | array element (`VH_KIND_ARRAY`) | `{arrayClass, int}` |
/// | byte-array view (`..._LE`/`..._BE`) | `{byte[], int}` |
///
/// `VH_KIND_MEMORY_SEGMENT` is deliberately absent: those handles are minted
/// in `phases_late/foreign_ffm.rs` with three slots and no metadata, so they
/// fall out through the unstamped path and the accessor refuses.
fn vh_coordinate_mirrors(ctx: &mut dyn NativeContext, vh: ObjectRef) -> Option<Vec<ObjectRef>> {
    // Unstamped handles have no answer at all — including the static case,
    // whose empty list must not be confused with "we know nothing".
    vh_var_type_mirror(ctx, vh)?;
    let kind = vh_kind(ctx, vh);
    let coord0 = vh_meta_mirror(ctx, vh, VH_COORD0);
    match kind {
        VH_KIND_ARRAY | VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE => {
            let arr = coord0?;
            let idx = ctx.primitive_class_mirror("int");
            Some(vec![arr, idx])
        }
        // Static field: zero coordinates, and that is the ANSWER, not a gap.
        1 => Some(Vec::new()),
        // Instance field.
        0 => Some(vec![coord0?]),
        _ => None,
    }
}

/// Build the `List<Class<?>>` `coordinateTypes()` returns.
///
/// `List.of(Object[])` is the JDK-owned immutable-list construction, so the
/// result `equals` the `List.of(...)` a caller compares against (the wave-2
/// corpus asserts exactly that). `Arrays.asList` is the synthetic-JDK
/// fallback, matching `lang_system.rs`'s `boxed_int_list`.
///
/// Every mirror is pinned across the allocations: `new_ref_array` and the
/// `List.of` invocation can both collect, and a raw `ObjectRef` held over a
/// collection is stale.
fn vh_class_list(ctx: &mut dyn NativeContext, mirrors: &[ObjectRef]) -> Option<Value> {
    let object_cid = ctx.class_id_by_name("java/lang/Object")?;
    let mut pins: Vec<usize> = Vec::with_capacity(mirrors.len());
    let mut base: Option<usize> = None;
    for m in mirrors {
        let h = ctx.pin_native_root(*m);
        if base.is_none() {
            base = Some(h);
        }
        pins.push(h);
    }
    let array = ctx.new_ref_array(object_cid, mirrors.len());
    let array_pin = ctx.pin_native_root(array);
    if base.is_none() {
        base = Some(array_pin);
    }
    let mut array = array;
    for (i, (m, h)) in mirrors.iter().zip(pins.iter()).enumerate() {
        let m = ctx.read_native_pin(*h, *m);
        array = ctx.read_native_pin(array_pin, array);
        ctx.set_array_element(array, i, Value::Object(Some(m)));
    }
    array = ctx.read_native_pin(array_pin, array);
    let list = ctx
        .invoke(
            "java/util/List",
            "of",
            "([Ljava/lang/Object;)Ljava/util/List;",
            &[Value::Object(Some(array))],
        )
        .ok()
        .flatten();
    let list = match list {
        Some(Value::Object(Some(_))) => list,
        _ => {
            let array = ctx.read_native_pin(array_pin, array);
            ctx.invoke(
                "java/util/Arrays",
                "asList",
                "([Ljava/lang/Object;)Ljava/util/List;",
                &[Value::Object(Some(array))],
            )
            .ok()
            .flatten()
        }
    };
    if let Some(b) = base {
        ctx.unpin_native_roots(b);
    }
    match list {
        Some(Value::Object(Some(_))) => list,
        _ => None,
    }
}

/// The refusal both accessors raise when the receiver carries no metadata.
///
/// There is no JDK failure mode for `varType()`/`coordinateTypes()` — every
/// real VarHandle can answer — so any exception here is a CratonVM deviation.
/// It is still the right one: the alternative is inventing a `Class`, and a
/// fabricated variable type is exactly the "plausible value where the VM does
/// not know" shape this corpus exists to find. `UnsupportedOperationException`
/// is catchable, names the receiver's kind, and cannot be mistaken for a real
/// answer.
fn vh_undescribable(what: &str, kind: i32) -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: format!(
            "VarHandle.{what}: this CratonVM VarHandle carries no variable/coordinate type \
             metadata (kind {kind}); it was minted by a factory that does not stamp it, so \
             the answer is unknown rather than absent"
        ),
    }
    .into()
}

pub(crate) fn register_p59_varhandle(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let vh = "java/lang/invoke/VarHandle";

    // get / getPlain / getOpaque — plain (relaxed) access
    let vh_get_plain_impl = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) < VH_NUM_FIELDS {
            return Ok(Some(ctx.get_array_element(this, 0)));
        }
        if vh_kind(ctx, this) == VH_KIND_MEMORY_SEGMENT {
            return Ok(Some(vh_memory_segment_get(ctx, this, args)));
        }
        if matches!(
            vh_kind(ctx, this),
            VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE
        ) {
            return Ok(Some(vh_byte_array_view_get(ctx, this, args)));
        }
        if vh_kind(ctx, this) == VH_KIND_ARRAY {
            vh_array_bounds_check(ctx, args)?;
            return Ok(Some(vh_array_get(ctx, args)));
        }
        let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
        let target = if is_static {
            None
        } else {
            match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            }
        };
        let val = vh_get(ctx, this, target);
        Ok(Some(vh_auto_box(ctx, val)?))
    };

    // getVolatile — SeqCst fence before + volatile read
    let vh_get_volatile_impl = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) < VH_NUM_FIELDS {
            return Ok(Some(ctx.get_array_element(this, 0)));
        }
        if vh_kind(ctx, this) == VH_KIND_MEMORY_SEGMENT {
            fence(Ordering::SeqCst);
            return Ok(Some(vh_memory_segment_get(ctx, this, args)));
        }
        if matches!(
            vh_kind(ctx, this),
            VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE
        ) {
            fence(Ordering::SeqCst);
            return Ok(Some(vh_byte_array_view_get(ctx, this, args)));
        }
        if vh_kind(ctx, this) == VH_KIND_ARRAY {
            vh_array_bounds_check(ctx, args)?;
            fence(Ordering::SeqCst);
            return Ok(Some(vh_array_get(ctx, args)));
        }
        let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
        let target = if is_static {
            None
        } else {
            match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            }
        };
        fence(Ordering::SeqCst);
        let val = vh_get_volatile(ctx, this, target);
        Ok(Some(vh_auto_box(ctx, val)?))
    };

    // getAcquire — plain read + Acquire fence after
    let vh_get_acquire_impl = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) < VH_NUM_FIELDS {
            return Ok(Some(ctx.get_array_element(this, 0)));
        }
        if vh_kind(ctx, this) == VH_KIND_MEMORY_SEGMENT {
            let v = vh_memory_segment_get(ctx, this, args);
            fence(Ordering::Acquire);
            return Ok(Some(v));
        }
        if matches!(
            vh_kind(ctx, this),
            VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE
        ) {
            let v = vh_byte_array_view_get(ctx, this, args);
            fence(Ordering::Acquire);
            return Ok(Some(v));
        }
        if vh_kind(ctx, this) == VH_KIND_ARRAY {
            vh_array_bounds_check(ctx, args)?;
            let v = vh_array_get(ctx, args);
            fence(Ordering::Acquire);
            return Ok(Some(v));
        }
        let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
        let target = if is_static {
            None
        } else {
            match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            }
        };
        let val = vh_get(ctx, this, target);
        fence(Ordering::Acquire);
        Ok(Some(vh_auto_box(ctx, val)?))
    };

    r.register(
        vh,
        "get",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_get_plain_impl,
    );
    r.register(
        vh,
        "getPlain",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_get_plain_impl,
    );
    r.register(
        vh,
        "getOpaque",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_get_plain_impl,
    );
    r.register(
        vh,
        "getVolatile",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_get_volatile_impl,
    );
    r.register(
        vh,
        "getAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_get_acquire_impl,
    );

    // set / setPlain / setOpaque — plain (relaxed) access
    let vh_set_plain_impl = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) < VH_NUM_FIELDS {
            let val = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_array_element(this, 0, val);
            return Ok(None);
        }
        if vh_kind(ctx, this) == VH_KIND_MEMORY_SEGMENT {
            vh_memory_segment_set(ctx, this, args);
            return Ok(None);
        }
        if matches!(
            vh_kind(ctx, this),
            VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE
        ) {
            vh_byte_array_view_set(ctx, this, args, 3);
            return Ok(None);
        }
        if vh_kind(ctx, this) == VH_KIND_ARRAY {
            vh_array_bounds_check(ctx, args)?;
            vh_array_set(ctx, args, 3);
            return Ok(None);
        }
        let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
        let (target, value) = if is_static {
            (None, args.get(1).copied().unwrap_or(Value::Object(None)))
        } else {
            let tgt = match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            (tgt, args.get(2).copied().unwrap_or(Value::Object(None)))
        };
        vh_set(ctx, this, target, value);
        Ok(None)
    };

    // setVolatile — volatile write + SeqCst fence after
    let vh_set_volatile_impl = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) < VH_NUM_FIELDS {
            let val = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_array_element(this, 0, val);
            return Ok(None);
        }
        if vh_kind(ctx, this) == VH_KIND_MEMORY_SEGMENT {
            vh_memory_segment_set(ctx, this, args);
            fence(Ordering::SeqCst);
            return Ok(None);
        }
        if matches!(
            vh_kind(ctx, this),
            VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE
        ) {
            vh_byte_array_view_set(ctx, this, args, 3);
            fence(Ordering::SeqCst);
            return Ok(None);
        }
        if vh_kind(ctx, this) == VH_KIND_ARRAY {
            vh_array_bounds_check(ctx, args)?;
            vh_array_set(ctx, args, 3);
            fence(Ordering::SeqCst);
            return Ok(None);
        }
        let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
        let (target, value) = if is_static {
            (None, args.get(1).copied().unwrap_or(Value::Object(None)))
        } else {
            let tgt = match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            (tgt, args.get(2).copied().unwrap_or(Value::Object(None)))
        };
        vh_set_volatile(ctx, this, target, value);
        fence(Ordering::SeqCst);
        Ok(None)
    };

    // setRelease — Release fence before + plain write
    let vh_set_release_impl = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) < VH_NUM_FIELDS {
            let val = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_array_element(this, 0, val);
            return Ok(None);
        }
        if vh_kind(ctx, this) == VH_KIND_MEMORY_SEGMENT {
            fence(Ordering::Release);
            vh_memory_segment_set(ctx, this, args);
            return Ok(None);
        }
        if matches!(
            vh_kind(ctx, this),
            VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE
        ) {
            fence(Ordering::Release);
            vh_byte_array_view_set(ctx, this, args, 3);
            return Ok(None);
        }
        if vh_kind(ctx, this) == VH_KIND_ARRAY {
            vh_array_bounds_check(ctx, args)?;
            fence(Ordering::Release);
            vh_array_set(ctx, args, 3);
            return Ok(None);
        }
        let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
        let (target, value) = if is_static {
            (None, args.get(1).copied().unwrap_or(Value::Object(None)))
        } else {
            let tgt = match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            (tgt, args.get(2).copied().unwrap_or(Value::Object(None)))
        };
        fence(Ordering::Release);
        vh_set(ctx, this, target, value);
        Ok(None)
    };

    r.register(vh, "set", "([Ljava/lang/Object;)V", vh_set_plain_impl);
    r.register(vh, "setPlain", "([Ljava/lang/Object;)V", vh_set_plain_impl);
    r.register(vh, "setOpaque", "([Ljava/lang/Object;)V", vh_set_plain_impl);
    r.register(
        vh,
        "setVolatile",
        "([Ljava/lang/Object;)V",
        vh_set_volatile_impl,
    );
    r.register(
        vh,
        "setRelease",
        "([Ljava/lang/Object;)V",
        vh_set_release_impl,
    );

    // compareAndSet(target?, expected, new) → boolean
    // compareAndExchangeAcquire / compareAndExchangeRelease / compareAndExchange → old value
    let vh_cas_impl = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) < VH_NUM_FIELDS {
            // Legacy: CAS on element 0 of the array
            let current = ctx.get_array_element(this, 0);
            let expected = args.get(1).copied().unwrap_or(Value::Object(None));
            let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
            let success = values_equal(&current, &expected);
            if success {
                ctx.set_array_element(this, 0, new_val);
            }
            return Ok(Some(Value::Int(if success { 1 } else { 0 })));
        }
        if matches!(
            vh_kind(ctx, this),
            VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE
        ) {
            let current = vh_byte_array_view_get(ctx, this, args);
            let expected = args.get(3).copied().unwrap_or(Value::Object(None));
            let new_val = args.get(4).copied().unwrap_or(Value::Object(None));
            let success = values_equal(&current, &expected);
            if success {
                let mut set_args = args.to_vec();
                if set_args.len() > 3 {
                    set_args[3] = new_val;
                }
                vh_byte_array_view_set(ctx, this, &set_args, 3);
            }
            return Ok(Some(Value::Int(if success { 1 } else { 0 })));
        }
        if vh_kind(ctx, this) == VH_KIND_ARRAY {
            vh_array_bounds_check(ctx, args)?;
            // args = [vh, array, idx, expected, new_val]
            let (arr, idx) = match vh_array_target(args) {
                Some(p) => p,
                None => return Ok(Some(Value::Int(0))),
            };
            let expected = args.get(3).copied().unwrap_or(Value::Object(None));
            let new_val = args.get(4).copied().unwrap_or(Value::Object(None));
            // `compare_and_swap_field` special-cases array receivers and does
            // the read/compare/write under the per-object CAS lock. The old
            // `get_array_element` + `set_array_element` pair had nothing
            // between the two, so two threads could both observe `expected`
            // and both report success -- a `compareAndSet` that provides no
            // mutual exclusion. Same defect the atomic-array natives carried
            // (see `util_concurrent_ext::atomic_array_cas`, 2026-07-27).
            let success = ctx.compare_and_swap_field(arr, idx, expected, new_val);
            return Ok(Some(Value::Int(if success { 1 } else { 0 })));
        }
        let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
        let (target, expected, new_val) = if is_static {
            (
                None,
                args.get(1).copied().unwrap_or(Value::Object(None)),
                args.get(2).copied().unwrap_or(Value::Object(None)),
            )
        } else {
            let tgt = match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            (
                tgt,
                args.get(2).copied().unwrap_or(Value::Object(None)),
                args.get(3).copied().unwrap_or(Value::Object(None)),
            )
        };
        // Instance fields go through the real CAS primitive; the plain
        // read-then-write this used to do let two threads both win the same
        // compareAndSet. Statics keep the old shape: there is no static-field
        // CAS on `NativeContext` to route them through.
        if let Some((holder, field_idx)) = vh_instance_slot(ctx, this, target) {
            let success = ctx.compare_and_swap_field(holder, field_idx, expected, new_val);
            return Ok(Some(Value::Int(if success { 1 } else { 0 })));
        }
        let current = vh_get(ctx, this, target);
        let success = values_equal(&current, &expected);
        if success {
            vh_set(ctx, this, target, new_val);
        }
        Ok(Some(Value::Int(if success { 1 } else { 0 })))
    };
    r.register(vh, "compareAndSet", "([Ljava/lang/Object;)Z", vh_cas_impl);
    r.register(
        vh,
        "weakCompareAndSet",
        "([Ljava/lang/Object;)Z",
        vh_cas_impl,
    );
    r.register(
        vh,
        "weakCompareAndSetPlain",
        "([Ljava/lang/Object;)Z",
        vh_cas_impl,
    );
    r.register(
        vh,
        "weakCompareAndSetAcquire",
        "([Ljava/lang/Object;)Z",
        vh_cas_impl,
    );
    r.register(
        vh,
        "weakCompareAndSetRelease",
        "([Ljava/lang/Object;)Z",
        vh_cas_impl,
    );

    // compareAndExchange — returns old value instead of boolean
    let vh_cae_impl = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) < VH_NUM_FIELDS {
            let current = ctx.get_array_element(this, 0);
            let expected = args.get(1).copied().unwrap_or(Value::Object(None));
            let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
            if values_equal(&current, &expected) {
                ctx.set_array_element(this, 0, new_val);
            }
            return Ok(Some(current));
        }
        if matches!(
            vh_kind(ctx, this),
            VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE
        ) {
            let current = vh_byte_array_view_get(ctx, this, args);
            let expected = args.get(3).copied().unwrap_or(Value::Object(None));
            let new_val = args.get(4).copied().unwrap_or(Value::Object(None));
            if values_equal(&current, &expected) {
                let mut set_args = args.to_vec();
                if set_args.len() > 3 {
                    set_args[3] = new_val;
                }
                vh_byte_array_view_set(ctx, this, &set_args, 3);
            }
            return Ok(Some(current));
        }
        if vh_kind(ctx, this) == VH_KIND_ARRAY {
            vh_array_bounds_check(ctx, args)?;
            let (arr, idx) = match vh_array_target(args) {
                Some(p) => p,
                None => return Ok(Some(Value::Object(None))),
            };
            let expected = args.get(3).copied().unwrap_or(Value::Object(None));
            let new_val = args.get(4).copied().unwrap_or(Value::Object(None));
            let current = ctx.get_array_element(arr, idx);
            if values_equal(&current, &expected) {
                ctx.set_array_element(arr, idx, new_val);
            }
            return Ok(Some(current));
        }
        let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
        let (target, expected, new_val) = if is_static {
            (
                None,
                args.get(1).copied().unwrap_or(Value::Object(None)),
                args.get(2).copied().unwrap_or(Value::Object(None)),
            )
        } else {
            let tgt = match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            (
                tgt,
                args.get(2).copied().unwrap_or(Value::Object(None)),
                args.get(3).copied().unwrap_or(Value::Object(None)),
            )
        };
        let current = vh_get(ctx, this, target);
        if values_equal(&current, &expected) {
            vh_set(ctx, this, target, new_val);
        }
        Ok(Some(current))
    };
    r.register(
        vh,
        "compareAndExchange",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_cae_impl,
    );
    r.register(
        vh,
        "compareAndExchangeAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_cae_impl,
    );
    r.register(
        vh,
        "compareAndExchangeRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_cae_impl,
    );

    // getAndSet — atomically swap, return old value
    let vh_get_and_set_impl = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if matches!(
            vh_kind(ctx, this),
            VH_KIND_BYTE_ARRAY_VIEW_LE | VH_KIND_BYTE_ARRAY_VIEW_BE
        ) {
            let old = vh_byte_array_view_get(ctx, this, args);
            vh_byte_array_view_set(ctx, this, args, 3);
            return Ok(Some(old));
        }
        if vh_kind(ctx, this) == VH_KIND_ARRAY {
            vh_array_bounds_check(ctx, args)?;
            let (arr, idx) = match vh_array_target(args) {
                Some(p) => p,
                None => return Ok(Some(Value::Object(None))),
            };
            let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
            let old = ctx.get_array_element(arr, idx);
            ctx.set_array_element(arr, idx, new_val);
            return Ok(Some(old));
        }
        let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
        let (target, new_val) = if is_static {
            (None, args.get(1).copied().unwrap_or(Value::Object(None)))
        } else {
            let tgt = match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            (tgt, args.get(2).copied().unwrap_or(Value::Object(None)))
        };
        let old = vh_get(ctx, this, target);
        vh_set(ctx, this, target, new_val);
        Ok(Some(old))
    };
    r.register(
        vh,
        "getAndSet",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_get_and_set_impl,
    );
    r.register(
        vh,
        "getAndSetAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_get_and_set_impl,
    );
    r.register(
        vh,
        "getAndSetRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        vh_get_and_set_impl,
    );

    // getAndAdd — atomically add int/long, return old value
    r.register(
        vh,
        "getAndAdd",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
            let (target, delta) = if is_static {
                (None, args.get(1).copied().unwrap_or(Value::Int(0)))
            } else {
                let tgt = match args.get(1) {
                    Some(Value::Object(o)) => *o,
                    _ => None,
                };
                (tgt, args.get(2).copied().unwrap_or(Value::Int(0)))
            };
            let old = vh_get(ctx, this, target);
            let new_val = match (&old, &delta) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(*b)),
                (Value::Long(a), Value::Long(b)) => Value::Long(a.wrapping_add(*b)),
                _ => old,
            };
            vh_set(ctx, this, target, new_val);
            Ok(Some(old))
        },
    );
    r.register(
        vh,
        "getAndAddAcquire",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
            let (target, delta) = if is_static {
                (None, args.get(1).copied().unwrap_or(Value::Int(0)))
            } else {
                let tgt = match args.get(1) {
                    Some(Value::Object(o)) => *o,
                    _ => None,
                };
                (tgt, args.get(2).copied().unwrap_or(Value::Int(0)))
            };
            let old = vh_get(ctx, this, target);
            let new_val = match (&old, &delta) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(*b)),
                (Value::Long(a), Value::Long(b)) => Value::Long(a.wrapping_add(*b)),
                _ => old,
            };
            vh_set(ctx, this, target, new_val);
            Ok(Some(old))
        },
    );
    r.register(
        vh,
        "getAndAddRelease",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let is_static = ctx.get_field(this, VH_IS_STATIC).as_int().unwrap_or(0) != 0;
            let (target, delta) = if is_static {
                (None, args.get(1).copied().unwrap_or(Value::Int(0)))
            } else {
                let tgt = match args.get(1) {
                    Some(Value::Object(o)) => *o,
                    _ => None,
                };
                (tgt, args.get(2).copied().unwrap_or(Value::Int(0)))
            };
            let old = vh_get(ctx, this, target);
            let new_val = match (&old, &delta) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(*b)),
                (Value::Long(a), Value::Long(b)) => Value::Long(a.wrapping_add(*b)),
                _ => old,
            };
            vh_set(ctx, this, target, new_val);
            Ok(Some(old))
        },
    );

    // MethodHandles VarHandle factory methods
    let mhs = "java/lang/invoke/MethodHandles";
    r.register(
        mhs,
        "arrayElementVarHandle",
        "(Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            // args[0] = the array Class mirror (a static factory: no receiver).
            // `varType()` is the COMPONENT type and the coordinates are
            // `{arrayClass, int}`; resolve both before allocating, and pin the
            // caller's mirror across the allocation.
            let arr_mirror = match args.first() {
                Some(Value::Object(Some(m))) => Some(*m),
                _ => None,
            };
            let comp_desc = arr_mirror
                .and_then(|m| crate::lang_class::mirror_class_name(ctx, m))
                .and_then(|name| name.strip_prefix('[').map(str::to_string));
            let comp_mirror = comp_desc
                .as_deref()
                .map(|d| crate::lang_class::descriptor_to_class_mirror(ctx, d));
            let pin = arr_mirror.map(|m| ctx.pin_native_root(m));
            let comp_pin = comp_mirror.map(|m| ctx.pin_native_root(m));
            let vh_obj =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_META_NUM_FIELDS)?;
            ctx.set_field(vh_obj, VH_CLASS_OR_TARGET, Value::Object(None));
            ctx.set_field(vh_obj, VH_FIELD_INDEX, Value::Int(0));
            // Mark this VarHandle as array-element kind so the get/set/cas
            // implementations interpret args[1]=array, args[2]=index.
            ctx.set_field(vh_obj, VH_IS_STATIC, Value::Int(VH_KIND_ARRAY));
            let arr_mirror = match (arr_mirror, pin) {
                (Some(m), Some(h)) => Some(ctx.read_native_pin(h, m)),
                _ => None,
            };
            let comp_mirror = match (comp_mirror, comp_pin) {
                (Some(m), Some(h)) => Some(ctx.read_native_pin(h, m)),
                _ => None,
            };
            // Only stamp when BOTH are known: a half-known handle would make
            // `coordinateTypes()` answer `{arrayClass}` — a plausible-looking
            // one-element list that is simply wrong.
            if let (Some(comp), Some(arr)) = (comp_mirror, arr_mirror) {
                vh_stamp_meta(ctx, vh_obj, Some(comp), Some(arr));
            }
            // `pin` was taken first, so releasing it releases `comp_pin` too.
            if let Some(h) = pin.or(comp_pin) {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(vh_obj))))
        },
    );
    r.register(
        mhs,
        "byteArrayViewVarHandle",
        "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            let elem = vh_byte_array_view_elem_from_mirror(ctx, args.get(0));
            let le = vh_byte_order_is_little(ctx, args.get(1));
            // The viewed element type and the fixed `{byte[], int}` coordinates
            // (JDK: `byteArrayViewVarHandle` handles are `(byte[], int)T`).
            let elem_desc = (elem as char).to_string();
            let var_type = crate::lang_class::descriptor_to_class_mirror(ctx, &elem_desc);
            let coord0 = crate::lang_class::descriptor_to_class_mirror(ctx, "[B");
            let vt_pin = ctx.pin_native_root(var_type);
            let c0_pin = ctx.pin_native_root(coord0);
            let vh_obj =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_META_NUM_FIELDS)?;
            ctx.set_field(vh_obj, VH_CLASS_OR_TARGET, Value::Int(elem as i32));
            ctx.set_field(
                vh_obj,
                VH_FIELD_INDEX,
                Value::Int(vh_byte_array_view_width(elem) as i32),
            );
            ctx.set_field(
                vh_obj,
                VH_IS_STATIC,
                Value::Int(if le {
                    VH_KIND_BYTE_ARRAY_VIEW_LE
                } else {
                    VH_KIND_BYTE_ARRAY_VIEW_BE
                }),
            );
            let var_type = ctx.read_native_pin(vt_pin, var_type);
            let coord0 = ctx.read_native_pin(c0_pin, coord0);
            vh_stamp_meta(ctx, vh_obj, Some(var_type), Some(coord0));
            ctx.unpin_native_roots(vt_pin);
            Ok(Some(Value::Object(Some(vh_obj))))
        },
    );

    // MethodHandles.Lookup VarHandle factory — instance field
    let lk = "java/lang/invoke/MethodHandles$Lookup";
    r.register(
        lk,
        "findVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            // args[0] = the Lookup, args[1] = holder Class mirror,
            // args[2] = field name String, args[3] = declared field type mirror.
            //
            // Everything is resolved BEFORE the allocation: `args` holds raw
            // `ObjectRef`s and `alloc_concurrent_synthetic` can collect.
            let holder_mirror = match args.get(1) {
                Some(Value::Object(Some(mirror))) => Some(*mirror),
                _ => None,
            };
            let type_mirror = match args.get(3) {
                Some(Value::Object(Some(mirror))) => Some(*mirror),
                _ => None,
            };
            let field_name = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            // A `Class` mirror's OWN heap class is `java.lang.Class`, so
            // `class_id_of_object` answered "java/lang/Class" for every call —
            // no field of that name is ever found and the index fell back to 0,
            // which is why every instance VarHandle in this VM pointed at the
            // receiver's FIRST field regardless of which field was asked for.
            // `mirror_class_id` is the reverse-map lookup that actually names
            // the represented class.
            let holder_class = holder_mirror.and_then(|m| crate::lang_class::mirror_class_id(ctx, m));
            // A mirror we could not resolve keeps the historical fallback
            // (ClassId 0, index 0) rather than raising: we do not know that the
            // field is absent, only that we cannot see the class. The handle it
            // yields is left UNSTAMPED, so `varType()` refuses instead of
            // reporting the caller's requested type as fact.
            let class_id = holder_class.unwrap_or_else(|| ClassId::new(0));
            let resolved = holder_class.and_then(|c| vh_find_instance_field(ctx, c, &field_name));
            if resolved.is_none()
                && holder_class.is_some_and(|c| vh_class_fields_visible(ctx, c))
            {
                // The JDK raises NoSuchFieldException here. Handing back a
                // handle aimed at field 0 is the fabricated-success shape: the
                // caller gets a working-looking VarHandle onto the wrong
                // variable.
                return Err(RuntimeError::NoSuchFieldException {
                    field_name: field_name.clone(),
                }
                .into());
            }
            let field_idx = resolved.map(|(_, i)| i as i32).unwrap_or(0);
            // `varType()` must be the field's REAL type. The mirror the caller
            // handed us is used only once it is confirmed to name that type —
            // then it is the identical `Class` object the caller will compare
            // against (`Integer.TYPE`, an `ldc`'d class constant), which a
            // freshly resolved mirror is not guaranteed to be.
            let declared =
                holder_class.and_then(|c| vh_instance_field_descriptor(ctx, c, &field_name));
            let requested = type_mirror
                .map(|m| crate::lang_invoke::mirror_to_descriptor(ctx, m).into_owned());
            let var_type = match (&declared, &requested, type_mirror) {
                (Some(d), Some(rq), Some(m)) if d == rq => Some(m),
                (Some(d), _, _) => Some(crate::lang_class::descriptor_to_class_mirror(ctx, d)),
                _ => None,
            };
            let vt_pin = var_type.map(|m| ctx.pin_native_root(m));
            let hm_pin = holder_mirror.map(|m| ctx.pin_native_root(m));
            let vh_obj =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_META_NUM_FIELDS)?;
            ctx.set_field(
                vh_obj,
                VH_CLASS_OR_TARGET,
                Value::Int(class_id.as_u32() as i32),
            );
            ctx.set_field(vh_obj, VH_FIELD_INDEX, Value::Int(field_idx));
            ctx.set_field(vh_obj, VH_IS_STATIC, Value::Int(0));
            let var_type = match (var_type, vt_pin) {
                (Some(m), Some(h)) => Some(ctx.read_native_pin(h, m)),
                _ => None,
            };
            let holder_mirror = match (holder_mirror, hm_pin) {
                (Some(m), Some(h)) => Some(ctx.read_native_pin(h, m)),
                _ => None,
            };
            // Both halves or neither: a coordinate list without a variable type
            // (or the reverse) would let one accessor answer while the other
            // invents.
            if let (Some(vt), Some(recv)) = (var_type, holder_mirror) {
                vh_stamp_meta(ctx, vh_obj, Some(vt), Some(recv));
            }
            // `vt_pin` was taken first, so releasing it releases `hm_pin` too.
            if let Some(h) = vt_pin.or(hm_pin) {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(vh_obj))))
        },
    );
    r.register(
        lk,
        "findStaticVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            // Same shape as `findVarHandle`, with two differences: the index is
            // a STATIC-block index (see `vh_find_static_field`), and a static
            // handle has NO coordinates, so only the variable type is stamped.
            let type_mirror = match args.get(3) {
                Some(Value::Object(Some(mirror))) => Some(*mirror),
                _ => None,
            };
            // Same `class_id_of_object` mis-read as `findVarHandle`: it answered
            // `java.lang.Class` for every mirror, so the static index was
            // computed against the wrong class.
            let holder_class = match args.get(1) {
                Some(Value::Object(Some(mirror))) => {
                    crate::lang_class::mirror_class_id(ctx, *mirror)
                }
                _ => None,
            };
            let class_id = holder_class.unwrap_or_else(|| ClassId::new(0));
            let field_name = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let resolved = holder_class.and_then(|c| vh_find_static_field(ctx, c, &field_name));
            if resolved.is_none()
                && holder_class.is_some_and(|c| vh_class_fields_visible(ctx, c))
            {
                return Err(RuntimeError::NoSuchFieldException {
                    field_name: field_name.clone(),
                }
                .into());
            }
            let field_idx = resolved.as_ref().map(|(i, _)| *i as i32).unwrap_or(0);
            let declared = resolved.map(|(_, d)| d);
            let requested = type_mirror
                .map(|m| crate::lang_invoke::mirror_to_descriptor(ctx, m).into_owned());
            let var_type = match (&declared, &requested, type_mirror) {
                (Some(d), Some(rq), Some(m)) if d == rq => Some(m),
                (Some(d), _, _) => Some(crate::lang_class::descriptor_to_class_mirror(ctx, d)),
                _ => None,
            };
            let vt_pin = var_type.map(|m| ctx.pin_native_root(m));
            let vh_obj =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_META_NUM_FIELDS)?;
            ctx.set_field(
                vh_obj,
                VH_CLASS_OR_TARGET,
                Value::Int(class_id.as_u32() as i32),
            );
            ctx.set_field(vh_obj, VH_FIELD_INDEX, Value::Int(field_idx));
            ctx.set_field(vh_obj, VH_IS_STATIC, Value::Int(1));
            if let (Some(m), Some(h)) = (var_type, vt_pin) {
                let m = ctx.read_native_pin(h, m);
                vh_stamp_meta(ctx, vh_obj, Some(m), None);
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(vh_obj))))
        },
    );
    // --- varType / coordinateTypes ---------------------------------------
    //
    // Both are CONCRETE JDK bytecode, so a registration alone is not enough on
    // the `vm_exec.rs` `check_override` route — see the record in
    // docs/known-issues/jdk-only/W6-1-varhandle-vartype-coordinatetypes.md.
    r.register(vh, "varType", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match vh_var_type_mirror(ctx, this) {
            Some(m) => Ok(Some(Value::Object(Some(m)))),
            None => Err(vh_undescribable("varType", vh_kind(ctx, this))),
        }
    });
    r.register(vh, "coordinateTypes", "()Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let Some(mirrors) = vh_coordinate_mirrors(ctx, this) else {
            return Err(vh_undescribable("coordinateTypes", vh_kind(ctx, this)));
        };
        match vh_class_list(ctx, &mirrors) {
            Some(list) => Ok(Some(list)),
            // The mirrors are known but no `List` implementation would build.
            // Returning null here would be a null `coordinateTypes()`, which
            // no JDK VarHandle ever answers.
            None => Err(vh_undescribable("coordinateTypes", vh_kind(ctx, this))),
        }
    });
    r.register(
        vh,
        "withInvokeExactBehavior",
        "()Ljava/lang/invoke/VarHandle;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        vh,
        "withInvokeBehavior",
        "()Ljava/lang/invoke/VarHandle;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.set_category(__prev_cat);
}

/// Deep equality for VarHandle CAS expected-value comparisons.
pub(crate) fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (Value::Object(x), Value::Object(y)) => x == y,
        _ => false,
    }
}

/// `Package.isCompatibleWith` — is dotted numeric version `spec` at least
/// `desired`? Component-wise comparison with missing components read as 0,
/// exactly as `java.lang.Package.isCompatibleWith` does. `None` means a
/// component would not parse as a non-negative int, which the real method
/// surfaces as `NumberFormatException`.
fn package_spec_at_least(spec: &str, desired: &str) -> Option<bool> {
    fn parts(v: &str) -> Option<Vec<u32>> {
        // `split('.')` on "" yields one empty component, which is exactly the
        // parse failure the real method reports.
        v.split('.').map(|c| c.parse::<u32>().ok()).collect()
    }
    let si = parts(spec)?;
    let di = parts(desired)?;
    for i in 0..si.len().max(di.len()) {
        let s = si.get(i).copied().unwrap_or(0);
        let d = di.get(i).copied().unwrap_or(0);
        if s < d {
            return Some(false);
        }
        if s > d {
            return Some(true);
        }
    }
    Some(true)
}

// =============================================================================
// java.lang.Package — 6-field synthetic
// (name=0, specTitle=1, specVersion=2, specVendor=3, implTitle=4, implVersion=5)
// =============================================================================

pub(crate) fn register_p59_package(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let pkg = "java/lang/Package";
    r.register(pkg, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        pkg,
        "getSpecificationTitle",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.register(
        pkg,
        "getSpecificationVersion",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    r.register(
        pkg,
        "getSpecificationVendor",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        },
    );
    r.register(
        pkg,
        "getImplementationTitle",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 4)))
        },
    );
    r.register(
        pkg,
        "getImplementationVersion",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 5)))
        },
    );
    // Real dotted-version comparison whenever the receiver actually carries a
    // specification version (slot 2). `Package.getPackage` below mints one
    // with a null slot 2, but `definePackage`-style callers and any future
    // manifest-fed builder can fill it, and for those the answer is
    // computable — so compute it instead of always saying "compatible".
    //
    // The null / empty spec-version case keeps the optimistic `true`: real
    // JDK throws NumberFormatException("Empty version string") there, but the
    // WildFly/JBoss-modules boot scan that motivated the `getPackages`
    // override calls this on exactly such a Package and would regress.
    r.register(
        pkg,
        "isCompatibleWith",
        "(Ljava/lang/String;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let spec = match ctx.get_field(this, 2) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            if spec.is_empty() {
                return Ok(Some(Value::Int(1)));
            }
            let desired = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            match package_spec_at_least(&spec, &desired) {
                Some(ok) => Ok(Some(Value::Int(i32::from(ok)))),
                // Real JDK propagates the Integer.parseInt failure verbatim.
                None => Err(MethodCallFailed::from(
                    RuntimeError::NumberFormatException {
                        message: format!("For input string: \"{desired}\""),
                    },
                )),
            }
        },
    );
    // KEEP: real `Package.isSealed()` is `sealBase != null`, and nothing in
    // CratonVM ever seals a package — `Package.getPackage` (below) and the
    // ClassLoader-side builders in `lang_class.rs` all mint Packages with no
    // seal base, exactly as the real JDK does for a package defined from a
    // manifest without a `Sealed` attribute. `false` is the computed answer
    // for every Package that can reach this native, not a placeholder.
    r.register(pkg, "isSealed", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(pkg, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let s = ctx.create_string(&format!("package {name}"));
        Ok(Some(Value::Object(Some(s))))
    });

    // Package.getPackage(String) — static
    r.register(
        pkg,
        "getPackage",
        "(Ljava/lang/String;)Ljava/lang/Package;",
        |ctx, args| {
            let name_ref = obj_arg(args, 0)?;
            let name = ctx.read_string(name_ref).unwrap_or_default();
            let pkg_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Package", 6)?;
            let s = ctx.create_string(&name);
            ctx.set_field(pkg_obj, 0, Value::Object(Some(s)));
            for i in 1..6 {
                ctx.set_field(pkg_obj, i, Value::Object(None));
            }
            Ok(Some(Value::Object(Some(pkg_obj))))
        },
    );

    // Package.getPackages() — static. Real-JDK bytecode delegates to
    // `ClassLoader.getClassLoader(Reflection.getCallerClass()).getPackages()`,
    // which in turn calls `packages().toArray(...)` with `packages()` returning
    // a Stream over the `packages` ConcurrentHashMap. In our boot, that path
    // routes back through bytecode which (in JDK 25) leaks a Stream object
    // where a `Package[]` is required (observed: `ReferencePipeline$Head`
    // returned from `ClassLoader.getPackages()[Ljava/lang/Package;`,
    // triggering NPE on arraylength in callers like
    // `org/jboss/modules/ConcurrentClassLoader.<clinit>` during WildFly boot).
    // Override with an empty `Package[]` — JBoss-modules only uses this for a
    // sanity scan and tolerates an empty result. Mirrors the existing
    // `ClassLoader.getDefinedPackages()` empty-array override.
    r.register(
        pkg,
        "getPackages",
        "()[Ljava/lang/Package;",
        |ctx, _args| {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(empty))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// StackWalker expansion — walk(), forEach(), getCallerClass(), StackFrame
// =============================================================================

/// `CRATONVM_SW_JDK_WALK=1` (`CRATONVM_COMPAT=stackwalker-jdk-walk`) -- serve
/// real-JDK `StackWalker.walk` / `forEach` from the JDK's OWN
/// `StackStreamFactory` bytecode instead of the natives below.
///
/// **Default OFF, on a measurement.** The JDK path pulls frames one BATCH at a
/// time and stops when the consumer stops, so on paper it should beat an
/// implementation that materialises every frame up front — and
/// `quartz-stackwalker-walk-is-38x-hotspot (retired 2026-08-25)` named it "the full fix,
/// and the only one on this page with the right ceiling". Built and measured
/// ABBA in one binary, on `probes/StackWalkerTerminationProbe.java`, it is
/// SLOWER at every depth (ms, 2000 iterations, two runs per arm):
///
/// | depth | eager native | JDK batched |
/// |---:|---|---|
/// | 2 | 706 / 740 | 3,039 / 4,386 |
/// | 40 | 4,748 / 6,400 | 12,453 / 14,609 |
/// | 120 | 25,384 / 30,024 | 37,459 / 41,980 |
///
/// The reason is that the JDK's laziness is written in Java: a reflective
/// `Constructor.newInstance` per buffer slot, an `Array.newInstance`, a
/// spliterator, the `doStackWalk` re-entry and a native call per frame for
/// `StackFrameBuffer.at`. Interpreted, that fixed cost is larger than the
/// per-frame cost it saves. On the one shape where batching CAN win — an early
/// `findFirst` at depth (`probes/StackWalkerFindFirstProbe.java`, `near`) — it
/// is inside the run-to-run spread.
///
/// Kept, rather than deleted, for three reasons: it is the arm that proves the
/// above, it exercises `lang_stackwalker.rs`'s `callStackWalk` /
/// `fetchStackFrames` (otherwise unreachable for `walk`, which is how three
/// real defects went unnoticed in it — see that file), and a future cheaper
/// `StackStreamFactory` would make it the better default.
fn stackwalker_jdk_walk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_SW_JDK_WALK").is_some())
}

pub(crate) fn register_p59_stackwalker(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sw = "java/lang/StackWalker";
    // `walk`/`forEach` are served by the natives below unless
    // `CRATONVM_SW_JDK_WALK=1` hands them to the JDK's own
    // `StackStreamFactory` bytecode — see `stackwalker_jdk_walk_enabled` for
    // the measurement that made the native the default. Synthetic-JDK mode has
    // no choice to make: the real `StackStreamFactory` may not be present at
    // all there, so the natives always register.
    if !r.real_jdk() || !stackwalker_jdk_walk_enabled() {
        // walk(Function<Stream<StackFrame>, R>) → R
        r.register(
            sw,
            "walk",
            "(Ljava/util/function/Function;)Ljava/lang/Object;",
            p59_sw_walk,
        );
        r.register(
            sw,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            p59_sw_for_each,
        );
    }
    r.register(
        sw,
        "getCallerClass",
        "()Ljava/lang/Class;",
        p59_sw_get_caller_class,
    );

    // StackFrame = 9-field synthetic — WP1.9 (+ WP1.10 slots 6-7):
    //
    // LAZY SLOTS (2, 5, 6). `populate_stack_frame` leaves these NULL and the
    // getters below build them on demand from slot 8. The three of them cost a
    // Java `String`, a Java `String` and a class-mirror resolve PER FRAME, and
    // a `filter(..).findFirst()` — the shape Mockito's `LocationImpl` runs on
    // every mock invocation — reads them for ONE frame. Measured on
    // `probes/StackWalkerFindFirstProbe.java`: the walk's profile is flat and
    // allocation-shaped (no symbol above 2.3%, ~24% in mimalloc's `mmap` with
    // no resolvable Rust caller), so the object COUNT is the lever, not any one
    // symbol. See `quartz-stackwalker-walk-is-38x-hotspot (retired 2026-08-25)`.
    //   slot 0: className (String, with '/' → '.')
    //   slot 1: methodName (String)
    //   slot 2: fileName (String or null)
    //   slot 3: lineNumber (Int, -1/-2 sentinels)
    //   slot 4: byteCodeIndex (Int, -1 for unknown/native)
    //   slot 5: declaringClassInternalName (String, '/'-form; used only by
    //           `toStackTraceElement()`'s formatting fallback) — LAZY
    //   slot 6: declaringClassMirror (Class or null) — LAZY, resolved from
    //           slot 8. It is resolved from the frame's OWN ClassId, never by
    //           name: a fresh by-name lookup can fail for a frame whose class
    //           is still running its own `<clinit>` (see
    //           `populate_stack_frame`'s doc comment). Deferring the resolve
    //           does not reintroduce that failure mode, because slot 8 carries
    //           the same guaranteed-valid id the eager resolve used.
    //   slot 7: retainClassRef (Int boolean) — copied from the owning walker so
    //           a StackFrame returned from walk() keeps its access contract
    //           after the user Function has returned.
    //   slot 8: declaringClassId (Int, -1 when the frame has no resolvable
    //           class) — the seed for every lazy slot above.
    let sf = "java/lang/StackWalker$StackFrame";
    r.register(sf, "getClassName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(sf, "getMethodName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(sf, "getFileName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(p59_frame_file_name(ctx, this)))
    });
    r.register(sf, "getLineNumber", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(sf, "getByteCodeIndex", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 4)))
    });
    // StackFrame.getDeclaringClass() returns its eagerly resolved mirror (slot
    // 6). RETAIN_CLASS_REFERENCE belongs to the frame, not the dynamic extent
    // of walk(): callers may legally return a StackFrame (or Optional holding
    // one) and inspect it after the Function has returned, so the permission is
    // read from the frame's own slot 7 (or the real carrier's flags bit).
    r.register(
        sf,
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Interface dispatch can route a real-JDK StackFrameInfo receiver
            // through this synthetic-carrier registration. Prefer the real
            // ClassFrameInfo.flags bit when present; otherwise use the
            // persistent permission copied onto our synthetic carrier.
            let retains_class_ref =
                crate::lang_stackwalker::class_frame_retains_class_ref(ctx, this).unwrap_or_else(
                    || matches!(ctx.get_field(this, 7), Value::Int(value) if value != 0),
                );
            if !retains_class_ref {
                return Err(RuntimeError::UnsupportedOperationException {
                    message: "No access to RETAIN_CLASS_REFERENCE".to_string(),
                }
                .into());
            }
            Ok(Some(p59_frame_mirror(ctx, this)))
        },
    );
    r.register(
        sf,
        "getMethodType",
        "()Ljava/lang/invoke/MethodType;",
        p59_sf_get_method_type_retain_checked,
    );
    r.register(sf, "isNativeMethod", "()Z", |ctx, args| {
        // Native iff lineNumber == -2 (per StackTraceElement convention).
        // This used to be a hard `false` with a dead `let _ = this;` and a
        // comment claiming the read "would require &mut" — `get_field` takes
        // `&self` and the callback already receives `&mut dyn NativeContext`,
        // so slot 3 (lineNumber, populated by `populate_stack_frame`) is
        // readable right here.
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(Some(Value::Int(0)));
        };
        let line = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        Ok(Some(Value::Int(if line == -2 { 1 } else { 0 })))
    });
    // `StackFrame.toString()`. The JDK's `StackFrameInfo.toString()` returns
    // `toStackTraceElement().toString()`; this synthetic carrier is not a real
    // class, so without an explicit registration it inherits `Object.toString`
    // and renders as `java.lang.StackWalker$StackFrame@1f3c`. That leaked
    // straight into user-visible output: Mockito's `LocationImpl` prints its
    // frame via `MetadataShim.toString()` → `StackFrame.toString()`, so every
    // "Wanted N times: -> at …" line named an identity hash instead of the
    // call site.
    r.register(sf, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let class_dotted = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => "Unknown".to_string(),
        };
        let method_name = match ctx.get_field(this, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => "unknown".to_string(),
        };
        let file_name = match p59_frame_file_name(ctx, this) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        };
        let line = match ctx.get_field(this, 3) {
            Value::Int(n) => n,
            _ => -1,
        };
        // Mirrors `StackTraceElement.toString()` (and `native_ste_to_string`):
        // `Cls.method(File:line)`, degrading to `(File)`, `(Native Method)`
        // and `(Unknown Source)` exactly as the JDK does.
        let loc = match (file_name.as_deref(), line) {
            (Some(f), n) if n >= 0 => format!("{f}:{n}"),
            (Some(f), _) => f.to_string(),
            (None, -2) => "Native Method".to_string(),
            _ => "Unknown Source".to_string(),
        };
        let s = format!("{class_dotted}.{method_name}({loc})");
        let result = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(result))))
    });
    r.register(
        sf,
        "toStackTraceElement",
        "()Ljava/lang/StackTraceElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Create a StackTraceElement from the synthetic StackFrame:
            // slot 0 = dotted class, 1 = method, 2 = file, 3 = line,
            // 5 = '/'-separated internal class name.
            let ste = try_alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4)?;
            let class_dotted = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let class_slashed = p59_frame_internal_name(ctx, this);
            let class_slashed = if class_slashed.is_empty() {
                class_dotted.replace('.', "/")
            } else {
                class_slashed
            };
            let method_name = match ctx.get_field(this, 1) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let file_name = match p59_frame_file_name(ctx, this) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            };
            let line = match ctx.get_field(this, 3) {
                Value::Int(n) => n,
                _ => -1,
            };
            crate::lang_misc::fill_stack_trace_element(
                ctx,
                ste,
                &class_slashed,
                &class_dotted,
                &method_name,
                file_name.as_deref(),
                line,
            );
            Ok(Some(Value::Object(Some(ste))))
        },
    );
    r.set_category(__prev_cat);
}

/// `StackWalker.StackFrame.getMethodType()`.
///
/// The 7-slot carrier stores no descriptor, but it does not need to: slot 5
/// holds the declaring class's INTERNAL name and slot 1 the method name, and
/// `NativeContext::declared_methods` has the descriptor for every method the
/// class declares. That is precisely how the sibling carrier's
/// `StackFrameInfo.getMethodType()` (`lang_stackwalker.rs`) already answers
/// this, so the earlier "there is literally nothing here to build a
/// MethodType from" note was wrong — this file just never used the same data.
///
/// Overload disambiguation follows the sibling: take the first declared
/// overload of that name. A `StackTraceEntry` carries no descriptor, and the
/// `getMethodType()` javadoc does not pin which overload is reported.
///
/// Resolution is done HERE rather than at `populate_stack_frame` time on
/// purpose: `declared_methods` materialises every method of the class, and
/// stack capture is hot (log4j's `StackLocator` walks on every logger
/// lookup) while `getMethodType()` is almost never called.
pub(crate) fn p59_sf_get_method_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    // NOTE: this helper is deliberately unguarded. `getDescriptor()` is
    // specified to work WITHOUT `RETAIN_CLASS_REFERENCE` and delegates through
    // here (see `register_real_jdk_stackwalker_frame_method_type`), so putting
    // the permission check in the shared helper made every `getDescriptor()`
    // call throw on a default walker. The check that the JDK does mandate for
    // `getMethodType()` lives in `p59_sf_get_method_type_retain_checked`, which
    // is what the `getMethodType` registrations bind.
    // TWO carriers reach this one function, and only one of them has the
    // 8-slot layout the registrar above documents.
    //
    //   * `populate_stack_frame` (this file) — the synthetic
    //     `java/lang/StackWalker$StackFrame`: slot 1 = methodName,
    //     slot 5 = declaring class internal name.
    //   * `lang_stackwalker::populate_sfi` — a REAL `java.lang.StackFrameInfo`,
    //     whose hierarchy-wide layout is
    //     `ClassFrameInfo{classOrMemberName(0), flags(1)}` then
    //     `StackFrameInfo{name(2), type(3), bci(4), contScope(5), ste(6)}`
    //     (`javap -p java.lang.StackFrameInfo java.lang.ClassFrameInfo`).
    //
    // Slot 5 is a deliberate alias that holds on BOTH: `populate_sfi` stashes
    // the '/'-form internal name in `contScope` on purpose (see its comment).
    // Slot 1 is NOT: on a real `StackFrameInfo` it is the `int flags` word, so
    // `read_string` refuses it, `method_name` came back empty, and
    // `getMethodType()` answered **null** / `getDescriptor()` threw
    // "descriptor metadata is unavailable" for every real-JDK frame.
    //
    // This is reachable in real-JDK mode, not just synthetic: the interface
    // registration in `register_real_jdk_stackwalker_frame_method_type` is on
    // the essential path, and `native_override::force_native_over_real_jdk_bytecode`
    // force-routes `("java/lang/StackWalker$StackFrame", "getMethodType"/
    // "getDescriptor")` over the real bytecode.
    //
    // Resolve `name` BY NAME first — the same order the sibling
    // `StackFrameInfo.getMethodType()` in `lang_stackwalker.rs` already uses.
    // A fabricated stub names its fields `_f0.._fN`, so the by-name read
    // misses there and the slot-1 fallback is taken automatically; no mode
    // flag is involved.
    let internal = p59_frame_internal_name(ctx, this);
    let method_name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => match ctx.get_field(this, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
    };
    if internal.is_empty() || method_name.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let Some(class_id) = ctx.class_id_by_name(&internal) else {
        return Ok(Some(Value::Object(None)));
    };
    let descriptor = ctx
        .declared_methods(class_id)
        .into_iter()
        .find(|m| m.name == method_name)
        .map(|m| m.descriptor);
    let Some(desc) = descriptor else {
        return Ok(Some(Value::Object(None)));
    };
    // Prefer the JDK factory when the real class library is present: it
    // interns, so the result compares `==` against MethodTypes obtained any
    // other way. Synthetic mode has no bytecode for it (and no native
    // registration either), so fall back to the Rust builder, which produces
    // the same JDK field layout.
    const FMDS: &str = "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/invoke/MethodType;";
    if ctx.method_exists(
        "java/lang/invoke/MethodType",
        "fromMethodDescriptorString",
        FMDS,
    ) {
        let desc_str = ctx.create_string(&desc);
        if let Ok(Some(Value::Object(Some(mt)))) = ctx.invoke(
            "java/lang/invoke/MethodType",
            "fromMethodDescriptorString",
            FMDS,
            &[Value::Object(Some(desc_str)), Value::Object(None)],
        ) {
            return Ok(Some(Value::Object(Some(mt))));
        }
    }
    Ok(Some(Value::Object(
        crate::lang_invoke::build_method_type_from_descriptor(ctx, &desc)?,
    )))
}

/// `StackFrame.getMethodType()` — the RETAIN_CLASS_REFERENCE-gated entry point.
///
/// Unlike `getDescriptor()`, `getMethodType()` is specified to throw
/// `UnsupportedOperationException` when the owning walker was not configured
/// with `Option.RETAIN_CLASS_REFERENCE`. The permission is read from the frame
/// itself (the real carrier's `flags` bit when the receiver is a real-JDK
/// `ClassFrameInfo`, otherwise the persistent copy in the synthetic carrier's
/// slot 7) rather than from transient walk state, because a frame may legally
/// outlive the `walk()` callback.
pub(crate) fn p59_sf_get_method_type_retain_checked(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let retains_class_ref = crate::lang_stackwalker::class_frame_retains_class_ref(ctx, this)
            .unwrap_or_else(|| matches!(ctx.get_field(this, 7), Value::Int(value) if value != 0));
        if !retains_class_ref {
            return Err(RuntimeError::UnsupportedOperationException {
                message: "No access to RETAIN_CLASS_REFERENCE".to_string(),
            }
            .into());
        }
    }
    p59_sf_get_method_type(ctx, args)
}

/// Promote only the real-JDK-safe StackWalker carrier accessors without
/// enabling synthetic reflection layouts in the default registry.
pub fn register_real_jdk_stackwalker_frame_method_type(r: &mut NativeMethodRegistry) {
    const SF: &str = "java/lang/StackWalker$StackFrame";
    r.register(
        SF,
        "getMethodType",
        "()Ljava/lang/invoke/MethodType;",
        p59_sf_get_method_type_retain_checked,
    );
    r.register(SF, "getDescriptor", "()Ljava/lang/String;", |ctx, args| {
        match p59_sf_get_method_type(ctx, args)? {
            Some(Value::Object(Some(method_type))) => {
                ctx.invoke_virtual(method_type, "descriptorString", "()Ljava/lang/String;", &[])
            }
            _ => Err(RuntimeError::UnsupportedOperationException {
                message: "StackWalker frame descriptor metadata is unavailable".into(),
            }
            .into()),
        }
    });
}

/// Populate the 8-slot StackFrame synthetic from a `StackTraceEntry`, including
/// the eagerly resolved declaring-class mirror and persistent retain-class bit.
/// Slot of the declaring-class `ClassId` on the p59 `StackFrame` carrier.
pub(crate) const P59_SF_CLASS_ID: usize = 8;
/// Number of slots the p59 `StackFrame` carrier declares.
pub(crate) const P59_SF_FIELDS: usize = 9;

/// The carrier's declaring-class id, or `None` when the frame had no resolvable
/// class (slot 8 holds `-1`) or the carrier predates the slot.
pub(crate) fn p59_frame_class_id(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
) -> Option<ClassId> {
    if ctx.object_num_fields(this) <= P59_SF_CLASS_ID {
        return None;
    }
    match ctx.get_field(this, P59_SF_CLASS_ID) {
        Value::Int(id) if id >= 0 => Some(ClassId::new(id as u32)),
        _ => None,
    }
}

/// Slot 2 (`fileName`), built on first read from slot 8 and memoized back into
/// the slot. A frame whose class declares no `SourceFile` answers null every
/// time, which is what an eagerly-populated carrier did for the same frame.
fn p59_frame_file_name(ctx: &mut dyn NativeContext, this: cratonvm_types::ObjectRef) -> Value {
    if let Value::Object(Some(s)) = ctx.get_field(this, 2) {
        return Value::Object(Some(s));
    }
    let Some(cid) = p59_frame_class_id(ctx, this) else {
        return Value::Object(None);
    };
    let Some(file) = ctx.class_source_file(cid) else {
        return Value::Object(None);
    };
    // GC-SAFETY: `create_string` allocates, so re-read `this` through a pin
    // before writing the slot back.
    let pin = ctx.pin_native_root(this);
    let s = ctx.create_string(&file);
    let s_pin = ctx.pin_native_root(s);
    let this = ctx.read_native_pin(pin, this);
    let s = ctx.read_native_pin(s_pin, s);
    ctx.set_field(this, 2, Value::Object(Some(s)));
    ctx.unpin_native_roots(pin);
    Value::Object(Some(s))
}

/// Slot 5 (the '/'-form declaring class name), built on first read from slot 8.
fn p59_frame_internal_name(ctx: &mut dyn NativeContext, this: cratonvm_types::ObjectRef) -> String {
    if let Value::Object(Some(s)) = ctx.get_field(this, 5) {
        return ctx.read_string(s).unwrap_or_default();
    }
    if let Some(cid) = p59_frame_class_id(ctx, this) {
        if let Some(name) = ctx.class_name_of_id(cid) {
            return name;
        }
    }
    // Last resort: the dotted name in slot 0. `toStackTraceElement` already
    // carried this fallback for a carrier whose slot 5 was empty.
    match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default().replace('.', "/"),
        _ => String::new(),
    }
}

/// Slot 6 (the declaring-class mirror), built on first read from slot 8 and
/// memoized back into the slot.
fn p59_frame_mirror(ctx: &mut dyn NativeContext, this: cratonvm_types::ObjectRef) -> Value {
    if let Value::Object(Some(m)) = ctx.get_field(this, 6) {
        return Value::Object(Some(m));
    }
    let Some(cid) = p59_frame_class_id(ctx, this) else {
        return Value::Object(None);
    };
    let pin = ctx.pin_native_root(this);
    let m = ctx.get_class_mirror(cid);
    let m_pin = ctx.pin_native_root(m);
    let this = ctx.read_native_pin(pin, this);
    let m = ctx.read_native_pin(m_pin, m);
    ctx.set_field(this, 6, Value::Object(Some(m)));
    ctx.unpin_native_roots(pin);
    Value::Object(Some(m))
}

pub(crate) fn populate_stack_frame(
    ctx: &mut dyn NativeContext,
    entry: &cratonvm_native_api::StackTraceEntry,
    retain_class_ref: bool,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // GC-SAFETY (see `lang_stackwalker::populate_sfi`): allocate every object
    // under a pin first, then read each back through its pin before the
    // (allocation-free) field writes. Holding the freshly-allocated `sf` and
    // strings in bare locals across the subsequent `create_string` calls is a
    // use-after-move/free under the moving collector.
    //
    // THREE allocations per frame, not six. `fileName` (slot 2), the '/'-form
    // class name (slot 5) and the declaring-class mirror (slot 6) are LAZY --
    // built by `p59_frame_file_name` / `p59_frame_internal_name` /
    // `p59_frame_mirror` from the `ClassId` in slot 8 on first read, and
    // memoized back into their slots. `getClassName()` and `getMethodName()`
    // stay eager because a stream predicate reads them for every frame it
    // visits, which is what makes them the only two worth paying for up front.
    //
    // Slot 8 carries `entry.class_id` -- the id captured directly off the live
    // interpreter `Frame` (see `stackwalker::entry_from_frame`), which is
    // always valid for a real frame. Deferring the mirror resolve is therefore
    // NOT the same as deferring to a by-name lookup: `class_id_by_name` is
    // loader-blind and ambiguity-strict and can miss for a frame whose class is
    // still executing its own `<clinit>` (observed: `SpringFactoriesLoader` /
    // `EntityManagerFactoryUtils` calling `LogFactory.getLog()` from their own
    // static initializers, walked by log4j-api's `StackLocator`, which NPE'd
    // when the lookup came back empty). The id is resolved once, here, exactly
    // as before; only the mirror LOOKUP moved.
    let decl_cid = entry
        .class_id
        .or_else(|| ctx.class_id_by_name(&entry.class_name));

    let mut sf =
        try_alloc_concurrent_synthetic(ctx, "java/lang/StackWalker$StackFrame", P59_SF_FIELDS)?;
    let base = ctx.pin_native_root(sf);
    // The dotted form comes from the shared per-ClassId cache when the class is
    // known, so a repeated frame does not re-run `.replace('/', '.')` and
    // allocate a fresh Rust `String` on top of the Java one.
    let dotted: std::sync::Arc<str> = match decl_cid {
        Some(cid) => crate::lang_class::dotted_class_name(ctx.vm_identity(), cid, &entry.class_name),
        None => std::sync::Arc::from(entry.class_name.replace('/', ".")),
    };
    let mut cls_str = ctx.create_string(&dotted);
    let h_cls = ctx.pin_native_root(cls_str);
    let mut meth_str = ctx.create_string(&entry.method_name);
    let h_meth = ctx.pin_native_root(meth_str);

    sf = ctx.read_native_pin(base, sf);
    cls_str = ctx.read_native_pin(h_cls, cls_str);
    ctx.set_field(sf, 0, Value::Object(Some(cls_str)));
    sf = ctx.read_native_pin(base, sf);
    meth_str = ctx.read_native_pin(h_meth, meth_str);
    ctx.set_field(sf, 1, Value::Object(Some(meth_str)));
    sf = ctx.read_native_pin(base, sf);
    ctx.set_field(sf, 2, Value::Object(None));
    ctx.set_field(sf, 3, Value::Int(entry.line_number));
    ctx.set_field(sf, 4, Value::Int(entry.byte_code_index));
    ctx.set_field(sf, 5, Value::Object(None));
    ctx.set_field(sf, 6, Value::Object(None));
    ctx.set_field(sf, 7, Value::Int(i32::from(retain_class_ref)));
    ctx.set_field(
        sf,
        P59_SF_CLASS_ID,
        Value::Int(decl_cid.map_or(-1, |c| c.as_u32() as i32)),
    );
    sf = ctx.read_native_pin(base, sf);
    ctx.unpin_native_roots(base);
    Ok(sf)
}

pub(crate) fn p59_sw_walk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let function = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Frame construction allocates heavily before the callback is invoked.
    // Keep both native arguments rooted and use the callback as the base pin
    // for the complete frame-array-stream graph.
    let pin_base = ctx.pin_native_root(function);
    let walker = args.first().and_then(|value| match value {
        Value::Object(Some(walker)) => Some(*walker),
        _ => None,
    });
    let walker_pin = walker.map(|walker| ctx.pin_native_root(walker));
    // Capture the current call stack and build a Stream<StackFrame>.
    // `capture_stack_trace` returns outer→inner (oldest frame first); a
    // `StackWalker` stream must be inner→outer (the walk()-caller first),
    // matching real JDK — reuse the same reversal + VM-internal-frame
    // stripping that `AbstractStackWalker.callStackWalk` already applies
    // (`lang_stackwalker::ordered_stack_walk_frames`) instead of handing
    // the caller the raw outer→inner order. Without this, `skip`/`limit`
    // chains over the stream (e.g. Lucene's `TestSecrets.ensureCaller`)
    // land on the wrong frame and misidentify the caller.
    let retain_class_ref = walker
        .map(|walker| {
            let walker = walker_pin
                .map(|pin| ctx.read_native_pin(pin, walker))
                .unwrap_or(walker);
            ctx.get_field_by_name(walker, "retainClassRef")
                .as_int()
                .unwrap_or(0)
                != 0
        })
        .unwrap_or(false);
    let raw_trace = ctx.capture_stack_trace(0); // key 0 = temporary
    let frames = crate::lang_stackwalker::ordered_stack_walk_frames(&raw_trace);
    let frame_count = frames.len();
    let arr = ctx.new_ref_array(ClassId::new(0), frame_count);
    // GC-SAFETY: `populate_stack_frame` allocates, so pin `arr` (which also
    // keeps its already-stored StackFrame elements reachable) and re-read the
    // forwarded reference before each `set_array_element`.
    let arr_pin = ctx.pin_native_root(arr);
    let mut arr = arr;
    for (i, entry) in frames.iter().enumerate() {
        let sf = populate_stack_frame(ctx, entry, retain_class_ref)?;
        let sf_pin = ctx.pin_native_root(sf);
        arr = ctx.read_native_pin(arr_pin, arr);
        let sf = ctx.read_native_pin(sf_pin, sf);
        ctx.set_array_element(arr, i, Value::Object(Some(sf)));
        ctx.unpin_native_roots(sf_pin);
    }
    let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)?;
    let stream_pin = ctx.pin_native_root(stream);
    arr = ctx.read_native_pin(arr_pin, arr);
    let stream = ctx.read_native_pin(stream_pin, stream);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    let stream = ctx.read_native_pin(stream_pin, stream);

    // Apply the Function argument to the stream: function.apply(stream)
    let function = ctx.read_native_pin(pin_base, function);
    let result = ctx.invoke_virtual(
        function,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(stream))],
    );
    ctx.unpin_native_roots(pin_base);
    result
}

pub(crate) fn p59_sw_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let pin_base = ctx.pin_native_root(consumer);
    let walker = args.first().and_then(|value| match value {
        Value::Object(Some(walker)) => Some(*walker),
        _ => None,
    });
    let walker_pin = walker.map(|walker| ctx.pin_native_root(walker));
    // Same inner→outer ordering as `p59_sw_walk`; copy the retain option onto
    // every frame so a consumer may safely retain it after this method returns.
    let retain_class_ref = walker
        .map(|walker| {
            let walker = walker_pin
                .map(|pin| ctx.read_native_pin(pin, walker))
                .unwrap_or(walker);
            ctx.get_field_by_name(walker, "retainClassRef")
                .as_int()
                .unwrap_or(0)
                != 0
        })
        .unwrap_or(false);
    let raw_trace = ctx.capture_stack_trace(0);
    let frames = crate::lang_stackwalker::ordered_stack_walk_frames(&raw_trace);
    let mut failure = None;
    for entry in &frames {
        let sf = populate_stack_frame(ctx, entry, retain_class_ref)?;
        let sf_pin = ctx.pin_native_root(sf);
        let consumer = ctx.read_native_pin(pin_base, consumer);
        let sf = ctx.read_native_pin(sf_pin, sf);
        let call = ctx.invoke_virtual(
            consumer,
            "accept",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(sf))],
        );
        ctx.unpin_native_roots(sf_pin);
        if let Err(error) = call {
            failure = Some(error);
            break;
        }
    }
    let result = match failure {
        Some(error) => Err(error),
        None => Ok(None),
    };
    ctx.unpin_native_roots(pin_base);
    result
}

pub(crate) fn p59_sw_get_caller_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Delegate to the canonical implementation in `stack_walker` so the
    // `@CallerSensitive` off-by-one logic lives in exactly one place.
    crate::stack_walker::native_get_caller_class(ctx, args)
}

// =============================================================================
// java.lang.Module stubs (Java 9+ module system)
// Module = 2-field (name=0, layer=1)
// ModuleLayer = 1-field (boot=0 boolean)
// =============================================================================

/// Helper: extract the module name string from a synthetic Module object (field 0).
/// Returns `""` (unnamed module sentinel) if the Module has no name set.
/// Target recorded for a dynamic `addExports`/`addOpens` edge whose target
/// `Module` argument is present but whose name CratonVM cannot resolve.
///
/// No module is ever called this, so the edge grants nobody — which is the
/// point. See [`dynamic_edge_target`].
pub(crate) const UNRESOLVED_TARGET_MODULE: &str = "cratonvm.unresolved-target-module";

/// Resolve the `target` string for a dynamic `addExports`/`addOpens` edge.
///
/// `ModuleRegistry::add_exports`/`add_opens` treat an EMPTY target as
/// *unqualified* — opened to every module in the process. [`read_module_name`]
/// also returns empty for the unnamed module. Those two meanings are not the
/// same thing, and conflating them silently turns a qualified grant into a
/// process-wide one.
///
/// That conflation was observable: Mockito's `InstrumentationMemberAccessor`
/// lazily calls `Instrumentation.redefineModule` to open `java.base/java.lang`
/// to *its own* ByteBuddy-injected dispatcher module. CratonVM cannot name that
/// module, so `read_module_name` returned `""` and the edge was stored as
/// "java.base opens java.lang to EVERYONE". `Module.isOpen("java.lang")` then
/// answered `true` where HotSpot answers `false`, and every later
/// encapsulation check keyed on that flag came apart — most visibly
/// `check_class_loader_define_class_is_encapsulated`, which stopped denying
/// Spring-CGLIB's `ReflectUtils` its reflective `ClassLoader.defineClass`. The
/// CGLIB AOP proxy then landed in the requested loader instead of its
/// superclass's, a package-private override stopped overriding (JVMS 5.4.5),
/// and `@MockitoSpyBean` stubs silently ran the real method
/// (`AotIntegrationTests#endToEndTestsForBeanOverrides`, 4 failures).
///
/// Measured against a real JDK 25 running the identical Mockito call: HotSpot
/// reports `isOpen("java.lang")` false, `isOpen("java.lang", <app unnamed>)`
/// false, and denies `setAccessible(ClassLoader.defineClass)` both before and
/// after. Recording a target CratonVM cannot name as "grants nobody" reproduces
/// that exactly, and can only ever grant LESS than HotSpot, never more.
///
/// `None` (no target argument at all — `Module.implAddOpens(String)`, or the
/// `--add-opens` CLI path) still means genuinely unqualified and is unchanged.
pub(crate) fn dynamic_edge_target(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    target_index: Option<usize>,
) -> String {
    let Some(idx) = target_index else {
        return String::new(); // no target argument => unqualified, as before
    };
    match args.get(idx) {
        Some(Value::Object(Some(m))) => {
            let name = read_module_name(ctx, *m);
            if name.is_empty() {
                UNRESOLVED_TARGET_MODULE.to_string()
            } else {
                name
            }
        }
        _ => String::new(),
    }
}

/// Read a `java.lang.Module`'s name. `""` is the unnamed-module sentinel, the
/// same one `ModuleRegistry` uses.
///
/// # Slot 0 is NOT the name on a real `java.lang.Module`
///
/// `javap -p java.lang.Module` (JDK 25) declares, in order:
///
/// ```text
///   private final java.lang.ModuleLayer layer;      // slot 0
///   private final java.lang.String name;            // slot 1
///   private final java.lang.ClassLoader loader;     // slot 2
///   private final java.lang.module.ModuleDescriptor descriptor;
/// ```
///
/// This helper read slot 0 unconditionally, so on every real-JDK `Module` it
/// read the `layer` field, `read_string` failed on a `ModuleLayer`, and it
/// answered `""` — i.e. *every named module looked like the unnamed module* to
/// every native that routes through here. `native_module_can_read` then asked
/// `ModuleRegistry::reads("", "")`, hit the unnamed-reader escape hatch and
/// returned `true`; `regression-suite/src/RJdkModule.java:114`
/// (`check(!svc.canRead(unnamed), ...)`) died on that, in both `--real-jdk`
/// and `--jdk-only`, with HotSpot answering `false`.
///
/// Prefer the declared `name` field and keep slot 0 only as the fallback, which
/// is where the synthetic 2-field `java/lang/Module` built by
/// `register_p59_module`'s `ModuleLayer.modules()` / `findModule` parks it (and
/// where `get_field_by_name` finds no such field, so the fallback is the one
/// that fires). This is the same two-step `jboss_jdkspecific::
/// module_registry_name` already uses — `build_module` there writes both `name`
/// and `layer` BY NAME for exactly this reason.
pub(crate) fn read_module_name(ctx: &dyn NativeContext, module_obj: ObjectRef) -> String {
    // Real-JDK layout: the declared `name` field. On a synthetic stand-in the
    // field does not exist and `get_field_by_name` answers `Object(None)`,
    // which is also what a genuinely unnamed module's null `name` answers — so
    // the slot-0 fallback below is tried in both cases, and is harmless in the
    // second (slot 0 is `layer`, and `read_string` refuses any object whose
    // class is known and is not `java/lang/String`).
    if let Value::Object(Some(s)) = ctx.get_field_by_name(module_obj, "name") {
        if let Some(name) = ctx.read_string(s) {
            return name;
        }
    }
    // Synthetic layout: slot 0 holds the name.
    if let Value::Object(Some(s)) = ctx.get_field(module_obj, 0) {
        if let Some(name) = ctx.read_string(s) {
            return name;
        }
    }
    String::new() // unnamed module
}

/// Helper: build a `HashSet<String>` Java object from a Vec of Rust strings.
pub(crate) fn build_string_set(ctx: &mut dyn NativeContext, items: Vec<String>) -> Result<ObjectRef, MethodCallFailed> {
    use cratonvm_types::ArrayElementType;
    let len = items.len();
    let arr = ctx.new_array(ArrayElementType::Reference, len);
    for (i, s) in items.iter().enumerate() {
        let js = ctx.create_string(s);
        ctx.set_array_element(arr, i, Value::Object(Some(js)));
    }
    let set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3)?;
    ctx.set_field(set, 0, Value::Object(Some(arr)));
    ctx.set_field(set, 1, Value::Int(len as i32));
    ctx.set_field(set, 2, Value::Int(16)); // initial capacity marker
    Ok(set)
}

/// `Module.canRead(Module)` → boolean.
///
/// A named top-level fn (not an inline closure) so it can be registered from
/// TWO places: `register_p59_module` below (the `synthetic-jdk`-feature-gated
/// path) and `register_essential_natives` (native-builtins/src/lib.rs, the
/// path the default `cratonvm-cli` build actually uses — `register_p59_module`
/// is unreachable there, since its only caller chain is entirely
/// `#[cfg(feature = "synthetic-jdk")]`-gated; see the `Module.getDescriptor`
/// registration in `register_essential_natives` (`lib.rs:12229`) or
/// `Module.canUse` in lib.rs for the fuller writeup of this recurring
/// essential-vs-synthetic-jdk gap. There is no fn named
/// `native_module_get_descriptor` — this pointer was stale; the essential
/// `getDescriptor` is an inline closure).
///
/// Unlike `getDescriptor`, real bytecode doesn't NPE for `canRead` — it
/// silently returns the WRONG boolean instead, which is easy to miss in a
/// suite scan. Confirmed via a standalone probe: on the default build (real
/// bytecode, this native unreachable), `someModule.canRead(javaBaseModule)`
/// returned `false`; real HotSpot returns `true` (every module implicitly
/// reads `java.base`, JVMS/JLS mandated). This native queries the boot
/// `ModuleRegistry`'s readability graph, which already seeds every module
/// with a read edge to java.base (`build_readability_graph`,
/// classloading/src/module.rs) — the logic itself was already correct, it
/// just wasn't reachable.
pub(crate) fn native_module_can_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let reader = read_module_name(ctx, this);
    let provider = match args.get(1) {
        Some(Value::Object(Some(m))) => read_module_name(ctx, *m),
        _ => return Ok(Some(Value::Int(0))),
    };
    let can = ctx.reads_module(&reader, &provider);
    Ok(Some(Value::Int(if can { 1 } else { 0 })))
}

/// `java/lang/Module.addExports(String, Module)` — same essential-vs-
/// synthetic-jdk gap as `canRead`/`getDescriptor` above: real bytecode
/// (`implAddExportsOrOpens`, Module.java) reads `this.descriptor.isOpen()`
/// directly (a field, not the overridden `getDescriptor()` accessor), and
/// that field is never populated on CratonVM's classpath-only Module
/// objects — so any direct call NPEs before ever reaching this native.
/// Registering here bypasses that bytecode entirely, exactly like `canRead`.
pub(crate) fn native_module_add_exports(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let module_name = read_module_name(ctx, this);
    let pkg_name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let target = dynamic_edge_target(ctx, args, Some(2));
    let pkg_slash = pkg_name.replace('.', "/");
    ctx.module_add_exports(&module_name, &pkg_slash, &target);
    Ok(Some(Value::Object(Some(this))))
}

/// `java/lang/Module.addOpens(String, Module)` — same gap as
/// `native_module_add_exports` immediately above.
pub(crate) fn native_module_add_opens(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let module_name = read_module_name(ctx, this);
    let pkg_name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let target = dynamic_edge_target(ctx, args, Some(2));
    let pkg_slash = pkg_name.replace('.', "/");
    ctx.module_add_opens(&module_name, &pkg_slash, &target);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn module_add_exports_or_opens_void(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    open: bool,
    target_index: Option<usize>,
) -> MethodCallResult {
    let target = dynamic_edge_target(ctx, args, target_index);
    module_add_exports_or_opens_void_to(ctx, args, open, &target)
}

/// The body of [`module_add_exports_or_opens_void`] with the target already
/// decided.
///
/// Split out for the `…ToAllUnnamed` entry points, whose target is not an
/// argument to read but the fixed token
/// [`cratonvm_classloading::module::ALL_UNNAMED_TARGET`]. They previously came
/// through the `target_index: None` path, which yields `""` — and `""` is
/// `ModuleRegistry`'s *unqualified* marker, i.e. "grant every module", which is
/// strictly more than the unnamed one HotSpot grants.
pub(crate) fn module_add_exports_or_opens_void_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    open: bool,
    target: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let module_name = read_module_name(ctx, this);
    let pkg_name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    let pkg_slash = pkg_name.replace('.', "/");
    if open {
        ctx.module_add_opens(&module_name, &pkg_slash, target);
    } else {
        ctx.module_add_exports(&module_name, &pkg_slash, target);
    }
    Ok(None)
}

pub(crate) fn native_module_impl_add_exports_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_add_exports_or_opens_void(ctx, args, false, None)
}

pub(crate) fn native_module_impl_add_exports_to_module(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_add_exports_or_opens_void(ctx, args, false, Some(2))
}

pub(crate) fn native_module_impl_add_opens_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_add_exports_or_opens_void(ctx, args, true, None)
}

pub(crate) fn native_module_impl_add_opens_to_module(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_add_exports_or_opens_void(ctx, args, true, Some(2))
}

/// `Module.implAddExportsToAllUnnamed(String)` — and, through the
/// `java.lang.System$1` (`JavaLangAccess`) bridge in `shared_secrets_bridge.rs`,
/// `JavaLangAccess.addExportsToAllUnnamed(Module, String)`.
///
/// Distinct from `implAddExports(String)`: "to all unnamed modules" is a
/// *qualified* edge, and HotSpot reports it as one —
/// `Module.isExported(pkg)` stays false after it. Sharing the
/// `implAddExports(String)` implementation (which records the unqualified
/// edge) is what this pair used to do.
pub(crate) fn native_module_impl_add_exports_to_all_unnamed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_add_exports_or_opens_void_to(ctx, args, false, ALL_UNNAMED_TARGET)
}

/// `Module.implAddOpensToAllUnnamed(String)` / `JavaLangAccess
/// .addOpensToAllUnnamed(Module, String)` — the `opens` half of
/// [`native_module_impl_add_exports_to_all_unnamed`].
pub(crate) fn native_module_impl_add_opens_to_all_unnamed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_add_exports_or_opens_void_to(ctx, args, true, ALL_UNNAMED_TARGET)
}

pub(crate) fn register_p59_module(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // =================================================================
    // java.lang.ModuleLayer
    // =================================================================
    let ml = "java/lang/ModuleLayer";

    // ModuleLayer.boot() → ModuleLayer
    //
    // WIDTH: 2, not 1. Neither number is the real JDK width — `javap -p
    // java.lang.ModuleLayer` on openjdk 25.0.3+9 declares SIX instance fields
    // (`cf`, `parents`, `nameToModule`, `allLayers`, `modules`,
    // `servicesCatalog`). 2 is what THIS VM declares for its synthetic
    // stand-in (`classloading/src/class_manager.rs:15403`,
    // `"java/lang/ModuleLayer" => instance_fields(2)`, fields literally named
    // `_f0`/`_f1`) and it is what the essential twin allocates
    // (`jboss_jdkspecific.rs:190`, `MODULE_LAYER_FIELD_COUNT = 2`, used by
    // `build_boot_layer`). This triple is in the p59/essential INTERSECTION
    // (`phases_late.rs`, `P59_AND_ESSENTIAL`): which body runs is decided by
    // the VM mode, not by the caller, so the two must stay interchangeable.
    //
    // The `1` was NOT an out-of-bounds hazard at the allocation, and saying so
    // would be wrong: `try_alloc_concurrent_synthetic` ends its success arm
    // with `let n = num_fields.max(real)` (util_concurrent_ext.rs:983), so an
    // under-request is clamped UP to the class's declared count and this
    // already produced a 2-slot object. What the `1` bought was a
    // `report_layout_alias("java/lang/ModuleLayer", 1, 2)` on EVERY call, via
    // `layout_alias::classify` — the mismatch detector firing on a mismatch
    // that was this file's own — plus a real hazard in the FAILURE arm, where
    // `refused_class(ctx, name, 1)` → `try_ensure_synthetic_class(name, 1)`
    // fabricates a genuinely 1-field `java/lang/ModuleLayer` class
    // (`fabricate_class` sets `num_total_fields: num_fields` verbatim,
    // class_manager.rs:4249) and a matching object, after which the twin's
    // `MODULE_LAYER_FIELD_COUNT` write is out of bounds by construction.
    //
    // Nothing reads slot 1 by index anywhere in the tree; `build_boot_layer`
    // addresses `parents`/`nameToModule`/`modules` by NAME. See
    // docs/known-issues/jdk-only/E20-R11-INTERSECTION-BLIND-GUARD-20260813.md §4.1.
    r.register(ml, "boot", "()Ljava/lang/ModuleLayer;", |ctx, _args| {
        let layer = try_alloc_concurrent_synthetic(ctx, "java/lang/ModuleLayer", 2)?;
        ctx.set_field(layer, 0, Value::Int(1)); // is boot
        Ok(Some(Value::Object(Some(layer))))
    });

    // ModuleLayer.modules() → Set<Module>
    // Returns Module objects for all registered modules in the boot layer,
    // MINUS every module whose only source is the application CLASS path.
    //
    // The registry is not the boot layer. `ClassManager::new` scans the app
    // class path for `module-info.class` and registers what it finds, so
    // `all_module_names()` includes modular jars that arrived on `-cp`. A real
    // JVM ignores such a `module-info` outright — the jar is an unnamed-module
    // citizen — so it is in neither `ModuleLayer.boot().modules()` nor
    // `findModule`. Measured on HotSpot 25 today with one jar in two positions
    // (scratchpad/e16, `com.e16.svc`, a javac-built modular jar with NO
    // `ModulePackages` attribute — the shape 38 of the corpus's 51 module-info
    // jars have):
    //
    // | jar position   | findModule | modules() contains | getModule().isNamed() |
    // |----------------|------------|--------------------|-----------------------|
    // | `-cp`          | false      | false              | false                 |
    // | `--module-path`| true       | true               | true                  |
    //
    // All three flip together; answering `true` here for a `-cp` jar while
    // `Class.getModule()` below answers the unnamed module is a state HotSpot
    // never produces, and it is what let `ServiceLoader` see one provider twice
    // (`JUnitException: Cannot create Launcher for multiple engines with the
    // same ID 'junit-jupiter'`).
    //
    // Same gate, same accessor, as `populate_boot_layer_modules`
    // (jboss_jdkspecific.rs) — deliberately NOT a second filter. Note this
    // surface's exposure is NOT the `ModulePackages`-dependent one:
    // `Class.getModule()` further down calls `module_name_of_class`, which
    // returns the class record's `module_name`, which `ClassManager` computed
    // once at define time from `ModuleRegistry::module_for_package`
    // (class_manager.rs:6178) — and a `module-info` with no `ModulePackages`
    // attribute registers ZERO packages, so that lookup misses and
    // `getModule()` answers unnamed. That accidental miss is what kept 13 of
    // the corpus's 14 double-source jars from tripping the JDK's `isNamed()`
    // guard. `all_module_names()` is the registry's KEY set: it misses nothing.
    // So this pair would report a `-cp` module present regardless of
    // `ModulePackages` — strictly wider exposure than the boot-layer path, not
    // narrower.
    //
    // See docs/known-issues/jdk-only/E4-R11-CLASS-PATH-MODULE-BOOT-LAYER-FIX-20260813.md
    // (§4.1 and NOM E-7) and docs/known-issues/jdk-only/E16-R11-P59-MODULE-LAYER-TWIN-20260813.md.
    r.register(ml, "modules", "()Ljava/util/Set;", |ctx, args| {
        use cratonvm_types::ArrayElementType;
        // A receiver carrying its OWN `nameToModule` map is its own authority,
        // and answering it from the boot `ModuleRegistry` is asking the wrong
        // object — the rule `native_module_layer_find_module` already states
        // for `findModule`. The essential twin
        // (`jboss_jdkspecific::native_module_layer_modules`) implements exactly
        // that arm, and it carries a SIDE EFFECT this body never had: it calls
        // `ServicesCatalog.create()`, `register(Module)`s every value of the
        // map into it, and writes the result back to the receiver's
        // `servicesCatalog` field (it also caches the derived set into
        // `modules`). `service_loader.rs:775-790` invokes
        // `ModuleLayer.modules()` for that side effect ALONE — it discards the
        // returned Set and then reads `layer.servicesCatalog` — so without it
        // `ServiceLoader.load(layer, service)` sees zero module-sourced
        // providers, which is the `module service providers: []` shape
        // `jboss_jdkspecific.rs:347-357` documents.
        //
        // CALL the twin; do not copy it. The gate below is the twin's own
        // entry condition, so the two bodies cannot disagree about when the
        // arm applies. Delegating UNCONDITIONALLY would be wrong: the twin's
        // fallback builds an EMPTY `java/util/HashSet` through its real
        // constructor, so a synthetic 2-slot boot layer would go from the
        // registry's module list to nothing.
        //
        // PREDICTED: inert as of today, and the reason is worth writing down
        // because it corrects the record this closes. In synthetic-jdk mode —
        // the ONLY mode this registrar runs in (`vm_init.rs:1932`) —
        // `java/lang/ModuleLayer` resolves to the fabricated stub
        // `class_manager.rs:15403` declares as `instance_fields(2)`, whose
        // fields are named `_f0`/`_f1`. `get_field_by_name`
        // (`vm_exec.rs:10980`) returns `Object(None)` for a name that does not
        // resolve, so the gate below is false — and so is the identical gate
        // inside the twin. The same fact makes `build_boot_layer`'s three
        // `set_field_by_name` writes no-ops. The runtime blocker on the
        // services catalog in synthetic mode is therefore the field NAMES, not
        // which registrar won: neither body would have written a catalog.
        // See docs/known-issues/jdk-only/E28-R11-P59-MODULE-WIDTHS-AND-CATALOG-20260813.md,
        // which nominates the `class_manager.rs` change that makes this live.
        if let Some(Value::Object(Some(layer))) = args.first() {
            if matches!(
                ctx.get_field_by_name(*layer, "nameToModule"),
                Value::Object(Some(_))
            ) {
                return crate::jboss_jdkspecific::native_module_layer_modules(ctx, args);
            }
        }
        let all = ctx.all_module_names();
        let names: Vec<String> = all
            .into_iter()
            .filter(|name| !ctx.module_is_class_path_only(name))
            .collect();
        let len = names.len();
        let arr = ctx.new_array(ArrayElementType::Reference, len);
        for (i, name) in names.iter().enumerate() {
            // 5: `class_manager.rs:15407` declares the synthetic
            // `java/lang/Module` as `instance_fields(5)` and the essential
            // twin's builder (`jboss_jdkspecific::build_module`) allocates
            // `MODULE_FIELD_COUNT = 5`. The object was already 5 slots wide
            // (`num_fields.max(real)`); asking 2 only fired
            // `report_layout_alias("java/lang/Module", 2, 5)` on every module
            // of every call.
            let m_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Module", 5)?;
            let name_str = ctx.create_string(name);
            ctx.set_field(m_obj, 0, Value::Object(Some(name_str)));
            // field 1 = layer — we don't set it here to avoid infinite recursion
            ctx.set_array_element(arr, i, Value::Object(Some(m_obj)));
        }
        let set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3)?;
        ctx.set_field(set, 0, Value::Object(Some(arr)));
        ctx.set_field(set, 1, Value::Int(len as i32));
        ctx.set_field(set, 2, Value::Int(16));
        Ok(Some(Value::Object(Some(set))))
    });

    // ModuleLayer.findModule(String) → Optional<Module>
    //
    // Empty for a modular jar that reached the registry only through the
    // application class path — see the `modules()` comment above for the
    // HotSpot 25 transcript both surfaces are matched against, and for why the
    // gate cannot be left off "because it is dead": this registrar OVERWRITES
    // `jboss_jdkspecific`'s filtered pair whenever it runs.
    r.register(
        ml,
        "findModule",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        |ctx, args| {
            // Same delegation, same gate, same reason as `modules()` above: a
            // layer that carries its own `nameToModule` is authoritative in
            // BOTH directions, and the essential twin
            // (`jboss_jdkspecific::native_module_layer_find_module`) is the
            // body that consults it — a miss there is a real absence, not a
            // reason to fall through to the boot registry. Also inert today
            // for the same reason (the synthetic stub's fields are `_f0`/`_f1`).
            if let Some(Value::Object(Some(layer))) = args.first() {
                if matches!(
                    ctx.get_field_by_name(*layer, "nameToModule"),
                    Value::Object(Some(_))
                ) {
                    return crate::jboss_jdkspecific::native_module_layer_find_module(ctx, args);
                }
            }
            let name_str = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            // Check if this module exists in the registry AND is a real module
            // of the boot layer rather than a `-cp` jar's ignored descriptor.
            let names = ctx.all_module_names();
            if names.iter().any(|n| n == &name_str) && !ctx.module_is_class_path_only(&name_str) {
                // 5 — see the `modules()` allocation above.
                let m_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Module", 5)?;
                let js = ctx.create_string(&name_str);
                ctx.set_field(m_obj, 0, Value::Object(Some(js)));
                ctx.set_field(opt, 0, Value::Object(Some(m_obj)));
            } else {
                ctx.set_field(opt, 0, Value::Object(None)); // empty Optional
            }
            Ok(Some(Value::Object(Some(opt))))
        },
    );

    // =================================================================
    // java.lang.Module
    // =================================================================
    let m = "java/lang/Module";

    // Module.getName() → String
    r.register(m, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    // Module.getLayer() → ModuleLayer
    r.register(m, "getLayer", "()Ljava/lang/ModuleLayer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    // Module.isNamed() → boolean
    r.register(m, "isNamed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let named = matches!(ctx.get_field(this, 0), Value::Object(Some(_)));
        Ok(Some(Value::Int(if named { 1 } else { 0 })))
    });

    // Module.toString() → String
    r.register(m, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => "unnamed".to_string(),
        };
        let s = ctx.create_string(&format!("module {name}"));
        Ok(Some(Value::Object(Some(s))))
    });

    // Module.getDescriptor() → ModuleDescriptor
    r.register(
        m,
        "getDescriptor",
        "()Ljava/lang/module/ModuleDescriptor;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // 16, not 2 — and this one is NOT a clamped floor. There is no
            // `java/lang/module/ModuleDescriptor` arm in
            // `class_manager.rs::synthetic_stub_fields`, so the fabricated stub
            // declares ZERO instance fields, `class_num_total_fields` answers
            // 0, and `num_fields.max(real)` leaves the request untouched: this
            // site produced genuinely 2-slot descriptors while all SEVEN other
            // allocation sites in the tree produce 16-slot ones
            // (`jboss_jdkspecific.rs:1642` `build_module_descriptor` — which is
            // what the essential `Module.getDescriptor` twin at `lib.rs:12229`
            // reaches through `build_synthetic_module_descriptor` — and `:1733`;
            // `reflect_annotations.rs:1672`, `:2048`, `:2110`, `:2178`). One
            // class, two object widths, decided by which registrar ran.
            //
            // Slots 0 (name) and 1 (flags) below keep their meaning; the
            // essential `isOpen` twin (`reflect_annotations.rs:1419`) reads
            // slot 1 UNGUARDED as its fallback, so uniform width is what keeps
            // that read in bounds for every descriptor regardless of origin.
            // The `layout_alias` report is NOT silenced by this — `classify(n,
            // 0)` reports `Undeclared` for any `n` — and the fix for that is a
            // declaration, nominated in
            // docs/known-issues/jdk-only/E28-R11-P59-MODULE-WIDTHS-AND-CATALOG-20260813.md.
            let desc =
                try_alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor", 16)?;
            ctx.set_field(desc, 0, ctx.get_field(this, 0)); // name
            ctx.set_field(desc, 1, Value::Int(0)); // flags
            Ok(Some(Value::Object(Some(desc))))
        },
    );

    // Module.getPackages() → Set<String>
    // Returns the real set of packages owned by this module from the registry.
    r.register(m, "getPackages", "()Ljava/util/Set;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let module_name = read_module_name(ctx, this);
        let packages = ctx.module_packages(&module_name);
        // Convert slash-format packages to dot-format for Java API.
        let dot_packages: Vec<String> = packages.into_iter().map(|p| p.replace('/', ".")).collect();
        let set = build_string_set(ctx, dot_packages)?;
        Ok(Some(Value::Object(Some(set))))
    });

    // Module.isExported(String) → boolean
    // Returns true if the package is exported unconditionally (to all modules).
    r.register(m, "isExported", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let module_name = read_module_name(ctx, this);
        let pkg_name = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => return Ok(Some(Value::Int(0))),
        };
        // Java API uses dot-format; registry uses slash-format.
        let pkg_slash = pkg_name.replace('.', "/");
        let exported = ctx.is_package_exported_unqualified(&module_name, &pkg_slash);
        Ok(Some(Value::Int(if exported { 1 } else { 0 })))
    });

    // Module.isExported(String, Module) → boolean
    r.register(
        m,
        "isExported",
        "(Ljava/lang/String;Ljava/lang/Module;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let module_name = read_module_name(ctx, this);
            let pkg_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Int(0))),
            };
            let to_module = match args.get(2) {
                Some(Value::Object(Some(m))) => read_module_name(ctx, *m),
                _ => String::new(),
            };
            let pkg_slash = pkg_name.replace('.', "/");
            let exported = ctx.is_package_exported_to(&module_name, &pkg_slash, &to_module);
            Ok(Some(Value::Int(if exported { 1 } else { 0 })))
        },
    );

    // Module.isOpen(String) → boolean
    r.register(m, "isOpen", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let module_name = read_module_name(ctx, this);
        let pkg_name = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => return Ok(Some(Value::Int(0))),
        };
        let pkg_slash = pkg_name.replace('.', "/");
        let open = ctx.is_package_open_unqualified(&module_name, &pkg_slash);
        Ok(Some(Value::Int(if open { 1 } else { 0 })))
    });

    // Module.isOpen(String, Module) → boolean
    r.register(
        m,
        "isOpen",
        "(Ljava/lang/String;Ljava/lang/Module;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let module_name = read_module_name(ctx, this);
            let pkg_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Int(0))),
            };
            let to_module = match args.get(2) {
                Some(Value::Object(Some(m))) => read_module_name(ctx, *m),
                _ => String::new(),
            };
            let pkg_slash = pkg_name.replace('.', "/");
            let open = ctx.is_package_open_to(&module_name, &pkg_slash, &to_module);
            Ok(Some(Value::Int(if open { 1 } else { 0 })))
        },
    );

    // Module.canRead(Module) → boolean
    // Queries the real readability graph in the ModuleRegistry.
    //
    // Same essential-vs-synthetic-jdk coverage gap as `getDescriptor` (see
    // `native_module_can_read`'s doc comment above; the `getDescriptor` twin
    // is the inline closure at `lib.rs:12229`, not a fn named
    // `native_module_get_descriptor` — no such fn exists): this closure is
    // registered here AND in `register_essential_natives`
    // (native-builtins/src/lib.rs) via the shared `native_module_can_read`
    // fn — this function's own registration only takes effect in
    // `synthetic-jdk`-feature builds. Unlike `getDescriptor`, the real
    // bytecode fallback doesn't NPE here (so this shipped silently wrong
    // rather than crashing) — confirmed via a standalone probe:
    // `test.mod.canRead(java.base)` returned `false` on the default build
    // (real bytecode) vs. `true` on real HotSpot (every module implicitly
    // reads java.base) and on this native (which correctly queries the
    // readability graph's mandated java.base edge).
    r.register(
        m,
        "canRead",
        "(Ljava/lang/Module;)Z",
        native_module_can_read,
    );

    // Module.addReads(Module) → Module (returns this)
    // Adds a dynamic read edge in the ModuleRegistry.
    r.register(
        m,
        "addReads",
        "(Ljava/lang/Module;)Ljava/lang/Module;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let reader = read_module_name(ctx, this);
            let provider = match args.get(1) {
                Some(Value::Object(Some(m))) => read_module_name(ctx, *m),
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            ctx.module_add_reads(&reader, &provider);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // Module.addExports(String, Module) → Module
    // Adds a dynamic export in the ModuleRegistry.
    r.register(
        m,
        "addExports",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;",
        native_module_add_exports,
    );

    // Module.addOpens(String, Module) → Module
    // Adds a dynamic open in the ModuleRegistry.
    r.register(
        m,
        "addOpens",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;",
        native_module_add_opens,
    );

    // =================================================================
    // java.lang.module.ModuleDescriptor
    // =================================================================
    let md = "java/lang/module/ModuleDescriptor";
    r.register(md, "name", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(md, "isAutomatic", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Int(v) = ctx.get_field_by_name(this, "automatic") {
            return Ok(Some(Value::Int(if v != 0 { 1 } else { 0 })));
        }
        Ok(Some(Value::Int(0)))
    });
    r.register(md, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Int(v) = ctx.get_field_by_name(this, "open") {
            return Ok(Some(Value::Int(if v != 0 { 1 } else { 0 })));
        }
        // Bit 0 of the synthetic flags field (index 1) marks "open" modules.
        if ctx.object_num_fields(this) > 1 {
            let flags = ctx.get_field(this, 1).as_int().unwrap_or(0);
            return Ok(Some(Value::Int(flags & 1)));
        }
        Ok(Some(Value::Int(0)))
    });

    // =================================================================
    // Class.getModule() → Module
    // =================================================================
    r.register(
        "java/lang/Class",
        "getModule",
        "()Ljava/lang/Module;",
        |ctx, args| {
            // Canonical Module per module name: the JDK compares Modules by
            // identity, so every class in a module must observe the SAME Module
            // instance. Mirror of the real-JDK getModule in lib.rs (HIB-CV-29).
            // Read the module of the class the mirror REFLECTS via
            // `class_id_from_mirror` — `class_id_of_object` would return
            // `java/lang/Class` and mis-report every class as java.base.
            let module_name: Option<String> =
                if let Some(Value::Object(Some(mirror))) = args.first() {
                    let class_id = ctx
                        .class_id_from_mirror(*mirror)
                        .unwrap_or_else(|| ctx.class_id_of_object(*mirror));
                    ctx.module_name_of_class(class_id)
                } else {
                    None
                };
            if let Some(cached) = ctx.get_cached_module_mirror(module_name.as_deref()) {
                return Ok(Some(Value::Object(Some(cached))));
            }
            // GC-safety: pin m_obj across the `create_string` allocation below;
            // otherwise a moving GC reclaims/relocates the unpinned local and
            // getModule() returns a stale ref (a reused slot → String →
            // `String.isNamed()` NoSuchMethodError). Same bug class as
            // reference_classloader_gc_root_gap.
            //
            // 5, per `class_manager.rs:15407` (`instance_fields(5)`) and
            // `jboss_jdkspecific.rs:214` (`MODULE_FIELD_COUNT = 5`). NOTE the
            // OTHER twin of this very triple, `lib.rs`'s real-JDK
            // `Class.getModule()`, still asks 2 — the declaration and the two
            // essential registrars do not agree with each other about this
            // class. 5 is the only number that is never an under-request, and
            // the object was 5 slots wide either way; see the nomination in
            // docs/known-issues/jdk-only/E28-R11-P59-MODULE-WIDTHS-AND-CATALOG-20260813.md.
            let m_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Module", 5)?;
            let pin = ctx.pin_native_root(m_obj);
            let module_name_val = module_name
                .as_deref()
                .map(|name| Value::Object(Some(ctx.create_string(name))))
                .unwrap_or(Value::Object(None));
            let m_obj = ctx.read_native_pin(pin, m_obj);
            // Dual-write: slot 0 = synthetic-Module contract; named `name` field =
            // what real `Module.isNamed()`/`getName()` bytecode reads (slot 0 is
            // `layer` in the real layout). `set_field_by_name` no-ops if absent.
            // See companion in lib.rs getModule (HIB-CV-29 isNamed follow-up).
            ctx.set_field(m_obj, 0, module_name_val);
            ctx.set_field_by_name(m_obj, "name", module_name_val);
            ctx.unpin_native_roots(pin);
            ctx.cache_module_mirror(module_name.as_deref(), m_obj);
            Ok(Some(Value::Object(Some(m_obj))))
        },
    );
    r.set_category(__prev_cat);
    ()
}

// =============================================================================
// java.lang.invoke.CallSite expansion — MutableCallSite, ConstantCallSite, VolatileCallSite
// CallSite = 1-field synthetic (target=0 MethodHandle)
// =============================================================================

// =============================================================================
// java.lang.Record expansion — components, equals, hashCode, toString stubs
// =============================================================================

// =============================================================================
// ProcessHandle expansion — children, descendants, onExit, info
// =============================================================================

pub(crate) fn p60_empty_optional(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let optional = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
    ctx.set_field(optional, 0, Value::Object(None));
    Ok(Some(Value::Object(Some(optional))))
}

// =============================================================================
// java.lang.reflect — Parameter, Executable, annotation enhancements
// =============================================================================

pub(crate) fn register_p61_reflect(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- Parameter = 3-field (name=0, modifiers=1, declaringExecutable=2) ---
    let param = "java/lang/reflect/Parameter";
    r.register(param, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(param, "getModifiers", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    // NOTE on the `Parameter` layout: the header above is stale.
    // `lang_reflect::build_parameter_array` is the only builder; it writes the
    // REAL JDK named fields (`name`/`modifiers`/`executable`/`index`) when the
    // real class is loaded and otherwise falls back to the 4-slot synthetic
    // layout [0]=name, [1]=modifiers, [2]=TYPE mirror, [3]=executable.
    //
    // `getType` is deliberately NOT re-registered here: this phase runs AFTER
    // `register_phase55_reflect`, whose slot-2 read is the correct answer for
    // that layout, and the constant null that used to sit here shadowed it.
    r.register(
        param,
        "getDeclaringExecutable",
        "()Ljava/lang/reflect/Executable;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Slot 3, not 2 — slot 2 is the parameter TYPE mirror, so the old
            // read handed callers a `Class` where an `Executable` is required
            // (JUnit 5's `ParameterContext` shim in `lib.rs` invokes this).
            match ctx.get_field_by_name(this, "executable") {
                Value::Object(Some(e)) => Ok(Some(Value::Object(Some(e)))),
                _ => Ok(Some(ctx.get_field(this, 3))),
            }
        },
    );
    // `Parameter.isVarArgs()` is true only for the LAST parameter of a varargs
    // executable (JDK: `executable.isVarArgs() && index == parameterCount-1`).
    // The constant `false` this replaces made every varargs tail look like an
    // ordinary array parameter to reflective callers.
    r.register(param, "isVarArgs", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The 4-slot synthetic fallback carries no `index`, so it genuinely
        // cannot answer this and keeps the conservative `false`.
        let Value::Int(index) = ctx.get_field_by_name(this, "index") else {
            return Ok(Some(Value::Int(0)));
        };
        let exec = match ctx.get_field_by_name(this, "executable") {
            Value::Object(Some(e)) => e,
            _ => match ctx.get_field(this, 3) {
                Value::Object(Some(e)) => e,
                _ => return Ok(Some(Value::Int(0))),
            },
        };
        // `invoke_virtual` can allocate and safepoint — pin `exec` across it.
        let pin = ctx.pin_native_root(exec);
        let varargs = matches!(
            ctx.invoke_virtual(exec, "isVarArgs", "()Z", &[]),
            Ok(Some(Value::Int(v))) if v != 0
        );
        if !varargs {
            ctx.unpin_native_roots(pin);
            return Ok(Some(Value::Int(0)));
        }
        let exec = ctx.read_native_pin(pin, exec);
        let count = match ctx.invoke_virtual(exec, "getParameterCount", "()I", &[]) {
            Ok(Some(Value::Int(c))) => c,
            _ => 0,
        };
        ctx.unpin_native_roots(pin);
        Ok(Some(Value::Int(if count > 0 && index == count - 1 {
            1
        } else {
            0
        })))
    });
    // `isNamePresent` is deliberately NOT registered here — see the matching
    // note in `register_phase55_reflect`. The constant `true` that used to sit
    // at this site shadowed `lang_reflect::native_parameter_is_name_present`,
    // which this phase runs after.
    r.register(
        param,
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(param, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => "arg".to_string(),
        };
        let s = ctx.create_string(&name);
        Ok(Some(Value::Object(Some(s))))
    });

    // --- Executable base methods ---
    let exec = "java/lang/reflect/Executable";
    // `getParameters` is deliberately NOT registered here. The empty array it
    // used to return shadowed `lang_reflect::native_executable_get_parameters`
    // (installed earlier, via `register_annotation_overrides` ->
    // `register_wp2_1_natives`), so under `--synthetic-jdk` every
    // `Executable.getParameters()` answered "this method takes no parameters".
    r.register(exec, "getParameterCount", "()I", |ctx, args| {
        // Was a constant 0. Count the descriptor's parameters instead — a
        // `Method`/`Constructor` mirror carries its descriptor in the metadata
        // tail these two readers know how to locate.
        let this = obj_arg(args, 0)?;
        let descriptor = match crate::lang_class::read_method_descriptor(ctx, this) {
            Some(desc) => desc,
            None => crate::lang_class::read_constructor_descriptor(ctx, this).unwrap_or_default(),
        };
        let (params, _) = crate::lang_class::parse_descriptor_param_and_return(&descriptor);
        Ok(Some(Value::Int(params.len() as i32)))
    });
    r.register(
        exec,
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        exec,
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // --- Field annotation methods (real implementations in lib.rs) ---
    let field = "java/lang/reflect/Field";
    r.register(
        field,
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        crate::lang_class::native_field_get_annotations,
    );
    r.register(
        field,
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        crate::lang_class::native_field_get_annotation,
    );
    r.register(
        field,
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
        crate::lang_class::native_field_is_annotation_present,
    );

    // --- Method annotation methods (real implementations in lib.rs) ---
    fn java_utf16_hash(s: &str) -> i32 {
        let mut h = 0i32;
        for ch in s.encode_utf16() {
            h = h.wrapping_mul(31).wrapping_add(ch as i32);
        }
        h
    }

    let method = "java/lang/reflect/Method";
    r.register(
        method,
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        crate::lang_class::native_method_get_annotations,
    );
    r.register(
        method,
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        crate::lang_class::native_method_get_annotation,
    );
    r.register(
        method,
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
        crate::lang_class::native_method_is_annotation_present,
    );
    r.register(method, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let Some((class_id, method_name, _method_desc)) =
            crate::lang_class::method_class_name_desc(ctx, this)
        else {
            return Ok(Some(Value::Int(0)));
        };
        let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
        Ok(Some(Value::Int(
            java_utf16_hash(&class_name.replace('/', ".")) ^ java_utf16_hash(&method_name),
        )))
    });
    r.register(method, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        if this == other {
            return Ok(Some(Value::Int(1)));
        }
        if ctx
            .class_name_arc_of_id(ctx.class_id_of_object(other))
            .as_deref()
            != Some("java/lang/reflect/Method")
        {
            return Ok(Some(Value::Int(0)));
        }
        let left = crate::lang_class::method_class_name_desc(ctx, this);
        let right = crate::lang_class::method_class_name_desc(ctx, other);
        Ok(Some(Value::Int(if left.is_some() && left == right {
            1
        } else {
            0
        })))
    });
    r.register(
        method,
        "getParameterAnnotations",
        "()[[Ljava/lang/annotation/Annotation;",
        crate::lang_class::native_method_get_parameter_annotations,
    );

    // --- Constructor annotation methods ---
    // Deliberately EMPTY now. This phase used to re-register
    // `Constructor.getAnnotations` as an empty array and
    // `Constructor.getAnnotation` as a constant null. Because phase 61 runs
    // AFTER `register_annotation_overrides`, those two constants shadowed the
    // real `native_method_get_annotations` / `native_method_get_annotation`
    // bindings — the ones that exist precisely so Jackson can see an
    // `@JsonCreator` constructor (see their comment in
    // `reflect_annotations.rs`). Leaving the slots alone lets the real
    // implementations stand in synthetic-jdk mode too.
    r.set_category(__prev_cat);
}

// =============================================================================
// java.lang.invoke.MethodHandles.Lookup — factory + lookup methods
// Lookup = 2-field (lookupClass=0, allowedModes=1)
// =============================================================================

// =============================================================================
// java.util.concurrent.ScheduledExecutorService — real delayed execution
// ScheduledThreadPoolExecutor = 2-field (corePoolSize=0, shutdown=1)
// =============================================================================

/// Convert a time value + TimeUnit argument to milliseconds.
/// TimeUnit ordinals: 0=NANOSECONDS, 1=MICROSECONDS, 2=MILLISECONDS, 3=SECONDS, 4=MINUTES, 5=HOURS, 6=DAYS
pub(crate) fn scheduled_convert_to_millis(
    ctx: &mut dyn NativeContext,
    value: i64,
    unit_arg: Option<&Value>,
) -> i64 {
    let ordinal = match unit_arg {
        Some(Value::Object(Some(unit_obj))) => {
            // TimeUnit stores ordinal in field 0
            ctx.get_field(*unit_obj, 0).as_int().unwrap_or(2) // default MILLISECONDS
        }
        _ => 2, // MILLISECONDS
    };
    match ordinal {
        0 => value / 1_000_000,  // NANOSECONDS
        1 => value / 1_000,      // MICROSECONDS
        2 => value,              // MILLISECONDS
        3 => value * 1_000,      // SECONDS
        4 => value * 60_000,     // MINUTES
        5 => value * 3_600_000,  // HOURS
        6 => value * 86_400_000, // DAYS
        _ => value,
    }
}

// =============================================================================
// MethodHandles extra factory methods
// =============================================================================

// =============================================================================
// Stream.mapMulti for all 4 stream types
// =============================================================================

pub(crate) fn register_p65_stream_map_multi(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // IntStream.mapMulti
    r.register(
        "java/util/stream/IntStream",
        "mapMulti",
        "(Ljava/util/function/IntConsumer;)Ljava/util/stream/IntStream;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // LongStream.mapMulti
    r.register(
        "java/util/stream/LongStream",
        "mapMulti",
        "(Ljava/util/function/LongConsumer;)Ljava/util/stream/LongStream;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // DoubleStream.mapMulti
    r.register(
        "java/util/stream/DoubleStream",
        "mapMulti",
        "(Ljava/util/function/DoubleConsumer;)Ljava/util/stream/DoubleStream;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // Map.ofEntries
    r.register(
        "java/util/Map",
        "ofEntries",
        "([Ljava/util/Map$Entry;)Ljava/util/Map;",
        |ctx, args| {
            let entries = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
                    let buckets = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
                    ctx.set_field(map, 0, Value::Object(Some(buckets)));
                    ctx.set_field(map, 1, Value::Int(0));
                    ctx.set_field(map, 2, Value::Int(16));
                    return Ok(Some(Value::Object(Some(map))));
                }
            };
            let len = ctx.array_length(entries);
            let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
            let cap = (len * 2).max(16);
            let buckets = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cap);
            ctx.set_field(map, 0, Value::Object(Some(buckets)));
            ctx.set_field(map, 1, Value::Int(0));
            ctx.set_field(map, 2, Value::Int(cap as i32));
            // We'd need to iterate entries and put each into the map
            // For now just set the size
            ctx.set_field(map, 1, Value::Int(len as i32));
            Ok(Some(Value::Object(Some(map))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.lang.constant — Constable, ConstantDesc interfaces (Java 12)
// =============================================================================

pub(crate) fn register_p66_constant_desc(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Constable interface. `describeConstable()` returns an `Optional`, never
    // null — the constant null this replaces NPE'd every `.isPresent()` /
    // `.orElseThrow()` caller. We have no nominal descriptor to hand back, so
    // answer the spec-legal negative: `Optional.empty()`.
    r.register(
        "java/lang/constant/Constable",
        "describeConstable",
        "()Ljava/util/Optional;",
        p60_empty_optional,
    );

    // ConstantDesc interface
    r.register(
        "java/lang/constant/ConstantDesc",
        "resolveConstantDesc",
        "(Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/Object;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // ClassDesc — Java 12
    let cd = "java/lang/constant/ClassDesc";
    r.register(
        cd,
        "of",
        "(Ljava/lang/String;)Ljava/lang/constant/ClassDesc;",
        |ctx, args| {
            // Return a wrapper around the name string
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/constant/ClassDesc", 1)?;
            ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cd,
        "of",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/constant/ClassDesc;",
        |ctx, args| {
            let pkg = match args.first() {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => String::new(),
            };
            let name = match args.get(1) {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => String::new(),
            };
            let full = if pkg.is_empty() {
                name
            } else {
                format!("{}.{}", pkg, name)
            };
            let s = ctx.create_string(&full);
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/constant/ClassDesc", 1)?;
            ctx.set_field(obj, 0, Value::Object(Some(s)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cd,
        "ofDescriptor",
        "(Ljava/lang/String;)Ljava/lang/constant/ClassDesc;",
        |ctx, args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/constant/ClassDesc", 1)?;
            ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(cd, "displayName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        cd,
        "descriptorString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    // This synthetic `ClassDesc` is a 1-field wrapper whose slot 0 holds the
    // string handed to `of`/`ofDescriptor`. Classify from that string instead
    // of answering three constants — the old `isPrimitive` was hard-`false`
    // even for `ClassDesc.ofDescriptor("I")`, and `isClassOrInterface` was
    // hard-`true` even for an array descriptor.
    fn cd_text(ctx: &mut dyn NativeContext, args: &[Value]) -> String {
        match args.first() {
            Some(Value::Object(Some(this))) => match ctx.get_field(*this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            },
            _ => String::new(),
        }
    }
    fn cd_is_primitive(text: &str) -> bool {
        // Only a bare single-character FIELD descriptor is primitive; a binary
        // name such as `java.lang.Integer` is not.
        text.len() == 1
            && matches!(
                text.as_bytes()[0],
                b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b'V'
            )
    }
    r.register(cd, "isArray", "()Z", |ctx, args| {
        let text = cd_text(ctx, args);
        Ok(Some(Value::Int(if text.starts_with('[') { 1 } else { 0 })))
    });
    r.register(cd, "isPrimitive", "()Z", |ctx, args| {
        let text = cd_text(ctx, args);
        Ok(Some(Value::Int(if cd_is_primitive(&text) { 1 } else { 0 })))
    });
    r.register(cd, "isClassOrInterface", "()Z", |ctx, args| {
        let text = cd_text(ctx, args);
        let is_cls = !text.is_empty() && !text.starts_with('[') && !cd_is_primitive(&text);
        Ok(Some(Value::Int(if is_cls { 1 } else { 0 })))
    });

    // MethodTypeDesc
    let mtd = "java/lang/constant/MethodTypeDesc";
    r.register(mtd, "of", "(Ljava/lang/constant/ClassDesc;[Ljava/lang/constant/ClassDesc;)Ljava/lang/constant/MethodTypeDesc;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/constant/MethodTypeDesc", 1)?;
        ctx.set_field(obj, 0, Value::Object(None));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        mtd,
        "ofDescriptor",
        "(Ljava/lang/String;)Ljava/lang/constant/MethodTypeDesc;",
        |ctx, args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/constant/MethodTypeDesc", 1)?;
            ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// Generic-type rendering for the synthetic reflection stubs
// =============================================================================

/// Render a `java.lang.reflect.Type` value to its canonical `getTypeName()`
/// string (e.g. `java.util.List<java.lang.String>[]`).
///
/// CratonVM's generic-signature reifier (`generics::type_sig_to_java`) builds
/// the parametric/array/var/wildcard cases as synthetic stub objects whose
/// class IS the bare interface name (`java/lang/reflect/ParameterizedType`
/// etc.). Those stubs have no `toString`, so a bare `Object.toString`
/// (`ParameterizedType@hash`) leaked out wherever the real
/// `sun.reflect…Impl.toString()` was expected — e.g. Spring's
/// `SerializableTypeWrapperTests.genericArrayType()` compares
/// `GenericArrayType.toString()` against `java.util.List<java.lang.String>[]`.
/// This renderer walks the synthetic stubs recursively and defers to the VM's
/// real `getTypeName()` for `Class` mirrors and real reifier impls.
pub(crate) fn render_type_name(ctx: &mut dyn NativeContext, val: &Value) -> String {
    let obj = match val {
        Value::Object(Some(o)) => *o,
        _ => return "?".to_string(),
    };
    let cid = ctx.class_id_of_object(obj);
    let cname = ctx.class_name_of_id(cid).unwrap_or_default();
    match cname.as_str() {
        "java/lang/reflect/ParameterizedType" => {
            let raw = ctx.get_field(obj, 0);
            let raw_s = render_type_name(ctx, &raw);
            let mut parts = Vec::new();
            if let Value::Object(Some(arr)) = ctx.get_field(obj, 1) {
                for i in 0..ctx.array_length(arr) {
                    let el = ctx.get_array_element(arr, i);
                    parts.push(render_type_name(ctx, &el));
                }
            }
            if parts.is_empty() {
                raw_s
            } else {
                format!("{}<{}>", raw_s, parts.join(", "))
            }
        }
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl" => {
            let raw = ctx.get_field_by_name(obj, "rawType");
            let raw_s = render_type_name(ctx, &raw);
            let mut parts = Vec::new();
            if let Value::Object(Some(arr)) = ctx.get_field_by_name(obj, "actualTypeArguments") {
                for i in 0..ctx.array_length(arr) {
                    let el = ctx.get_array_element(arr, i);
                    parts.push(render_type_name(ctx, &el));
                }
            }
            if parts.is_empty() {
                raw_s
            } else {
                format!("{}<{}>", raw_s, parts.join(", "))
            }
        }
        "org/springframework/core/ResolvableType$SyntheticParameterizedType" => {
            let raw = ctx.get_field_by_name(obj, "rawType");
            let raw_s = render_type_name(ctx, &raw);
            let mut parts = Vec::new();
            if let Value::Object(Some(arr)) = ctx.get_field_by_name(obj, "typeArguments") {
                for i in 0..ctx.array_length(arr) {
                    let el = ctx.get_array_element(arr, i);
                    parts.push(render_type_name(ctx, &el));
                }
            }
            if parts.is_empty() {
                raw_s
            } else {
                format!("{}<{}>", raw_s, parts.join(", "))
            }
        }
        "java/lang/reflect/GenericArrayType" => {
            let comp = ctx.get_field(obj, 0);
            format!("{}[]", render_type_name(ctx, &comp))
        }
        "sun/reflect/generics/reflectiveObjects/GenericArrayTypeImpl" => {
            let comp = ctx.get_field_by_name(obj, "genericComponentType");
            format!("{}[]", render_type_name(ctx, &comp))
        }
        "java/lang/reflect/TypeVariable" => match ctx.get_field(obj, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "?".to_string()),
            _ => "?".to_string(),
        },
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl" => {
            match ctx.get_field_by_name(obj, "name") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "?".to_string()),
                _ => "?".to_string(),
            }
        }
        "java/lang/reflect/WildcardType" => {
            // upperBounds=0, lowerBounds=1
            if let Value::Object(Some(arr)) = ctx.get_field(obj, 1) {
                if ctx.array_length(arr) > 0 {
                    let el = ctx.get_array_element(arr, 0);
                    return format!("? super {}", render_type_name(ctx, &el));
                }
            }
            if let Value::Object(Some(arr)) = ctx.get_field(obj, 0) {
                if ctx.array_length(arr) > 0 {
                    let el = ctx.get_array_element(arr, 0);
                    let s = render_type_name(ctx, &el);
                    if s != "java.lang.Object" {
                        return format!("? extends {}", s);
                    }
                }
            }
            "?".to_string()
        }
        // Same rendering for the REAL reifier impl, which is what
        // `generics::real_wildcard_type` actually builds — the positional arm
        // above only covers the bare-interface synthetic. Without this the
        // fall-through arm below asked a `WildcardTypeImpl` for
        // `getTypeName()`, which it does not have, so `List<? extends Number>`
        // rendered as
        // `java.util.List<sun.reflect.generics.reflectiveObjects.WildcardTypeImpl@6>`
        // and logged a `NoSuchMethodError` on the way. Reads by name, matching
        // how that builder populates the object.
        "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl" => {
            if let Value::Object(Some(arr)) = ctx.get_field_by_name(obj, "lowerBounds") {
                if ctx.array_length(arr) > 0 {
                    let el = ctx.get_array_element(arr, 0);
                    return format!("? super {}", render_type_name(ctx, &el));
                }
            }
            if let Value::Object(Some(arr)) = ctx.get_field_by_name(obj, "upperBounds") {
                if ctx.array_length(arr) > 0 {
                    let el = ctx.get_array_element(arr, 0);
                    let s = render_type_name(ctx, &el);
                    if s != "java.lang.Object" {
                        return format!("? extends {s}");
                    }
                }
            }
            "?".to_string()
        }
        // Class mirror or a real reifier impl: ask the VM for getTypeName(),
        // falling back to toString() and finally the dotted class name.
        _ => {
            // `obj` is a parameter held across both calls; the first allocates
            // a String and can move it before the second dereferences it.
            let obj_pin = ctx.pin_native_root(obj);
            if let Ok(Some(Value::Object(Some(s)))) =
                ctx.invoke_virtual(obj, "getTypeName", "()Ljava/lang/String;", &[])
            {
                if let Some(rendered) = ctx.read_string(s) {
                    return rendered;
                }
            }
            let obj = ctx.read_native_pin(obj_pin, obj);
            if let Ok(Some(Value::Object(Some(s)))) =
                ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[])
            {
                if let Some(rendered) = ctx.read_string(s) {
                    return rendered;
                }
            }
            cname.replace('/', ".")
        }
    }
}

// =============================================================================
// java.lang.runtime.SwitchBootstraps — Java 21 (pattern matching runtime)
// =============================================================================

pub(crate) fn register_p69_switch_bootstraps(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // ESCALATED, not justified — the previous note here was factually wrong.
    //
    // What IS confirmed: `invokedynamic` never reaches these registrations.
    // `vm/src/runtime/invokedynamic.rs` dispatches on `info.bsm_class` /
    // `info.bsm_method` (see SWITCH_BOOTSTRAPS / OBJECT_METHODS at lines
    // 54/63, used at 316-320) and bootstraps both families inside the VM, so
    // the only route in is an explicit reflective call.
    //
    // What was WRONG: HotSpot has no "the caller does not get a usable
    // CallSite" contract. A reflective `SwitchBootstraps.typeSwitch(...)` on
    // a real JDK returns a working `ConstantCallSite`, and
    // `ObjectMethods.bootstrap` returns a MethodHandle or a Class depending
    // on the requested name. `null` is a fabricated answer, not a spec'd one.
    //
    // Why it is still null: implementing this means calling the VM-side
    // bootstrappers (`invokedynamic::bootstrap_type_switch` /
    // `bootstrap_enum_switch` / the ObjectMethods path), which take
    // `&SharedVm`, `&mut JvmThread` and an `IndyInfo` built from the caller's
    // constant pool. `NativeContext` exposes no equivalent, and a faithful
    // argument-validating version would also have to throw NPE/IAE on the
    // all-null arguments that `vm/src/vm.rs::switch_bootstraps_stub_p69`
    // currently asserts return null. Both are cross-crate changes.
    let sb = "java/lang/runtime/SwitchBootstraps";
    r.register(sb, "typeSwitch",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;",
        |_ctx, _args| Ok(Some(Value::Object(None))));
    r.register(sb, "enumSwitch",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;",
        |_ctx, _args| Ok(Some(Value::Object(None))));

    // ObjectMethods (record support)
    let om = "java/lang/runtime/ObjectMethods";
    r.register(om, "bootstrap",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/TypeDescriptor;Ljava/lang/Class;Ljava/lang/String;[Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))));
    r.set_category(__prev_cat);
}

// =============================================================================
// java.lang.invoke.ConstantBootstraps — Java 11
// =============================================================================

pub(crate) fn register_p70_constant_bootstraps(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cb = "java/lang/invoke/ConstantBootstraps";

    // `nullConstant` is not purely constant: the real body is
    //   `if (requireNonNull(type).isPrimitive())
    //        throw new IllegalArgumentException("not reference: " + type);
    //    return null;`
    // The primitive guard is implemented here — a caller asking for a null of
    // `int.class` has a bug that must surface as IAE, not as a null that
    // unbox-NPEs somewhere else. The `requireNonNull(type)` arm is
    // deliberately NOT implemented: `vm/src/vm.rs::constant_bootstraps_
    // null_constant_p70` calls this with a null type and asserts null back,
    // and that test lives in another crate (escalated).
    //
    // The condy opcode never reaches here anyway — `interpreter.rs` resolves
    // `ConstantBootstraps.nullConstant` in-VM (~19533) — so this serves
    // explicit reflective calls only.
    r.register(cb, "nullConstant",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        |ctx, args| {
            if let Some(Value::Object(Some(type_mirror))) = args.get(2).copied() {
                let primitive = matches!(
                    ctx.invoke_virtual(type_mirror, "isPrimitive", "()Z", &[]),
                    Ok(Some(Value::Int(n))) if n != 0
                );
                if primitive {
                    let rendered = match ctx.invoke_virtual(
                        type_mirror,
                        "getName",
                        "()Ljava/lang/String;",
                        &[],
                    ) {
                        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                        _ => String::new(),
                    };
                    return Err(MethodCallFailed::from(RuntimeError::IllegalArgumentException {
                        message: format!("not reference: {rendered}"),
                    }));
                }
            }
            Ok(Some(Value::Object(None)))
        });

    // primitiveClass — returns the Class object for a primitive type
    r.register(cb, "primitiveClass",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Class;",
        |ctx, args| {
            // args[1] = name (String), e.g. "int", "long", "double"
            let name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            // Map primitive name to wrapper class and get the TYPE field
            let wrapper_class = match name.as_str() {
                "int" => "java/lang/Integer",
                "long" => "java/lang/Long",
                "double" => "java/lang/Double",
                "float" => "java/lang/Float",
                "boolean" => "java/lang/Boolean",
                "byte" => "java/lang/Byte",
                "char" => "java/lang/Character",
                "short" => "java/lang/Short",
                "void" => "java/lang/Void",
                _ => return Ok(Some(Value::Object(None))),
            };
            // Get the TYPE static field from the wrapper class
            let class_id = ctx.ensure_class_initialized(wrapper_class)
                .map_err(|e| RuntimeError::IllegalStateException {
                    message: format!("Failed to initialize {}: {:?}", wrapper_class, e),
                })?;
            if let Some(field_idx) = ctx.resolve_field_index(wrapper_class, "TYPE") {
                let type_val = ctx.get_static_field(class_id, field_idx);
                return Ok(Some(type_val));
            }
            // Fallback: return the class mirror itself
            let mirror = ctx.get_class_mirror(class_id);
            Ok(Some(Value::Object(Some(mirror))))
        });

    // enumConstant — look up enum constant by name from the declaring class
    r.register(cb, "enumConstant",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Enum;",
        |ctx, args| {
            // args[1] = constant name, args[2] = enum Class
            let const_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let enum_class = match args.get(2) {
                Some(Value::Object(Some(c))) => *c,
                _ => return Ok(Some(Value::Object(None))),
            };
            let class_id = ctx.class_id_of_object(enum_class);
            let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
            // Look up the static field with the enum constant name
            if let Some(field_idx) = ctx.resolve_field_index(&class_name, &const_name) {
                let val = ctx.get_static_field(class_id, field_idx);
                return Ok(Some(val));
            }
            Ok(Some(Value::Object(None)))
        });

    // getStaticFinal — read a static final field from a declaring class
    r.register(cb, "getStaticFinal",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/Object;",
        |ctx, args| {
            // args[1] = field name, args[3] = declaring class
            let field_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let declaring = match args.get(3) {
                Some(Value::Object(Some(c))) => *c,
                _ => {
                    // If no declaring class, use the type class (args[2])
                    match args.get(2) {
                        Some(Value::Object(Some(c))) => *c,
                        _ => return Ok(Some(Value::Object(None))),
                    }
                }
            };
            let class_id = ctx.class_id_of_object(declaring);
            let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
            if let Some(field_idx) = ctx.resolve_field_index(&class_name, &field_name) {
                let val = ctx.get_static_field(class_id, field_idx);
                return Ok(Some(val));
            }
            Ok(Some(Value::Object(None)))
        });

    // invoke — invoke a MethodHandle with provided arguments
    r.register(cb, "invoke",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;[Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            // args[3] = MethodHandle, args[4] = Object[] arguments
            let mh = match args.get(3) {
                Some(Value::Object(Some(m))) => *m,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Gather arguments from the Object[] array
            let mut invoke_args = vec![Value::Object(Some(mh))];
            if let Some(Value::Object(Some(arr))) = args.get(4) {
                let len = ctx.array_length(*arr);
                for i in 0..len {
                    invoke_args.push(ctx.get_array_element(*arr, i));
                }
            }
            // Try invoking the MethodHandle
            match ctx.invoke_virtual(mh, "invoke", "([Ljava/lang/Object;)Ljava/lang/Object;", &invoke_args) {
                Ok(result) => Ok(result),
                Err(_) => Ok(Some(Value::Object(None))),
            }
        });

    // fieldVarHandle — create a VarHandle for an instance field
    r.register(cb, "fieldVarHandle",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            // args[1]=name, args[3]=declaringClass, args[4]=fieldType
            let field_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let vh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", 6)?;
            ctx.set_field(vh, 0, Value::Int(0)); // kind=instance
            let name_s = ctx.create_string(&field_name);
            ctx.set_field(vh, 1, Value::Object(Some(name_s)));
            ctx.set_field(vh, 2, args.get(3).copied().unwrap_or(Value::Object(None))); // declaring class
            ctx.set_field(vh, 3, args.get(4).copied().unwrap_or(Value::Object(None))); // field type
            // Resolve field index for fast access
            if let Some(Value::Object(Some(decl))) = args.get(3) {
                let class_id = ctx.class_id_of_object(*decl);
                let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
                let idx = ctx.resolve_field_index(&class_name, &field_name).unwrap_or(0);
                ctx.set_field(vh, 4, Value::Int(idx as i32));
            }
            Ok(Some(Value::Object(Some(vh))))
        });

    // staticFieldVarHandle — create a VarHandle for a static field
    r.register(cb, "staticFieldVarHandle",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            let field_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let vh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", 6)?;
            ctx.set_field(vh, 0, Value::Int(1)); // kind=static
            let name_s = ctx.create_string(&field_name);
            ctx.set_field(vh, 1, Value::Object(Some(name_s)));
            ctx.set_field(vh, 2, args.get(3).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(vh, 3, args.get(4).copied().unwrap_or(Value::Object(None)));
            if let Some(Value::Object(Some(decl))) = args.get(3) {
                let class_id = ctx.class_id_of_object(*decl);
                let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
                let idx = ctx.resolve_field_index(&class_name, &field_name).unwrap_or(0);
                ctx.set_field(vh, 4, Value::Int(idx as i32));
                ctx.set_field(vh, 5, Value::Int(class_id.as_u32() as i32));
            }
            Ok(Some(Value::Object(Some(vh))))
        });

    // arrayVarHandle — create a VarHandle for array element access
    r.register(cb, "arrayVarHandle",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            let vh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", 6)?;
            ctx.set_field(vh, 0, Value::Int(2)); // kind=array
            ctx.set_field(vh, 3, args.get(3).copied().unwrap_or(Value::Object(None))); // component type
            Ok(Some(Value::Object(Some(vh))))
        });
    r.set_category(__prev_cat);
}

#[cfg(test)]
mod dynamic_edge_target_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};

    /// Build a `java.lang.Module` whose field 0 (the name) is either a String
    /// or null. Null is how CratonVM represents both the unnamed module and a
    /// module it simply cannot name.
    fn module_obj(ctx: &mut MockNativeContext, name: Option<&str>) -> Value {
        let cid = ctx.ensure_class_initialized("java/lang/Module").unwrap();
        let m = ctx.alloc_object(cid, 4);
        match name {
            Some(n) => {
                let s = ctx.create_string(n);
                ctx.set_field(m, 0, Value::Object(Some(s)));
            }
            None => ctx.set_field(m, 0, Value::Object(None)),
        }
        Value::Object(Some(m))
    }

    #[test]
    fn no_target_argument_stays_unqualified() {
        let mut ctx = mock_ctx();
        // `Module.implAddOpens(String)` and the `--add-opens` CLI path have no
        // target module at all; those really are unqualified.
        assert_eq!(dynamic_edge_target(&mut ctx, &[], None), "");
    }

    #[test]
    fn named_target_module_is_recorded_verbatim() {
        let mut ctx = mock_ctx();
        let target = module_obj(&mut ctx, Some("jdk.compiler"));
        let args = [Value::Object(None), Value::Object(None), target];
        assert_eq!(
            dynamic_edge_target(&mut ctx, &args, Some(2)),
            "jdk.compiler"
        );
    }

    /// The regression guard. A target module CratonVM cannot name must NOT
    /// collapse to the empty string: `ModuleRegistry::add_opens` reads an empty
    /// target as *unqualified*, i.e. opened to every module in the process.
    ///
    /// Mockito's `InstrumentationMemberAccessor` opens `java.base/java.lang` to
    /// its own ByteBuddy-injected module; widening that to "everyone" made
    /// `Module.isOpen("java.lang")` answer true where HotSpot answers false and
    /// broke `AotIntegrationTests#endToEndTestsForBeanOverrides`.
    #[test]
    fn unnameable_target_module_is_not_recorded_as_unqualified() {
        let mut ctx = mock_ctx();
        let target = module_obj(&mut ctx, None);
        let args = [Value::Object(None), Value::Object(None), target];
        let resolved = dynamic_edge_target(&mut ctx, &args, Some(2));
        assert!(
            !resolved.is_empty(),
            "an empty target means unqualified/open-to-everyone"
        );
        assert_eq!(resolved, UNRESOLVED_TARGET_MODULE);
    }
}
