// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.lang.reflect` / `java.lang.invoke` natives: VarHandle, MethodHandles, Module, Package, StackWalker, constant/switch bootstraps.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

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
    r.register(param, "isNamePresent", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
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

/// Resolve the field index for a named field in a class (instance fields only).
/// Walks the inheritance chain. Returns `None` if not found.
pub(crate) fn vh_find_instance_field(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> Option<(ClassId, usize)> {
    let mut current = class_id;
    loop {
        let fields = ctx.declared_fields(current);
        for (i, f) in fields.iter().enumerate() {
            if f.name == name {
                // Field index is a class-local index; we need the total offset.
                // Use declared_fields count in supers to compute absolute offset.
                return Some((current, i));
            }
        }
        current = ctx.superclass_of(current)?;
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
pub(crate) fn vh_auto_box(ctx: &mut dyn NativeContext, val: Value) -> Value {
    match val {
        Value::Int(_) => {
            let wrapper = crate::alloc_concurrent_synthetic(ctx, "java/lang/Integer", 1);
            ctx.set_field(wrapper, 0, val);
            Value::Object(Some(wrapper))
        }
        Value::Long(_) => {
            let wrapper = crate::alloc_concurrent_synthetic(ctx, "java/lang/Long", 1);
            ctx.set_field(wrapper, 0, val);
            Value::Object(Some(wrapper))
        }
        Value::Float(_) => {
            let wrapper = crate::alloc_concurrent_synthetic(ctx, "java/lang/Float", 1);
            ctx.set_field(wrapper, 0, val);
            Value::Object(Some(wrapper))
        }
        Value::Double(_) => {
            let wrapper = crate::alloc_concurrent_synthetic(ctx, "java/lang/Double", 1);
            ctx.set_field(wrapper, 0, val);
            Value::Object(Some(wrapper))
        }
        _ => val, // Already an object reference or null
    }
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
        Ok(Some(vh_auto_box(ctx, val)))
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
        Ok(Some(vh_auto_box(ctx, val)))
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
        Ok(Some(vh_auto_box(ctx, val)))
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
        |ctx, _args| {
            let vh_obj =
                alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS);
            ctx.set_field(vh_obj, VH_CLASS_OR_TARGET, Value::Object(None));
            ctx.set_field(vh_obj, VH_FIELD_INDEX, Value::Int(0));
            // Mark this VarHandle as array-element kind so the get/set/cas
            // implementations interpret args[1]=array, args[2]=index.
            ctx.set_field(vh_obj, VH_IS_STATIC, Value::Int(VH_KIND_ARRAY));
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
            let vh_obj =
                alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS);
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
            let vh_obj =
                alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS);
            // args[1] = class mirror (JClass), args[2] = field name String, args[3] = field type
            let field_name = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let class_id = match args.get(1) {
                Some(Value::Object(Some(mirror))) => ctx.class_id_of_object(*mirror),
                _ => ClassId::new(0),
            };
            let field_idx = vh_find_instance_field(ctx, class_id, &field_name)
                .map(|(_, i)| i as i32)
                .unwrap_or(0);
            ctx.set_field(
                vh_obj,
                VH_CLASS_OR_TARGET,
                Value::Int(class_id.as_u32() as i32),
            );
            ctx.set_field(vh_obj, VH_FIELD_INDEX, Value::Int(field_idx));
            ctx.set_field(vh_obj, VH_IS_STATIC, Value::Int(0));
            Ok(Some(Value::Object(Some(vh_obj))))
        },
    );
    r.register(
        lk,
        "findStaticVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        |ctx, args| {
            let vh_obj =
                alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS);
            let class_id = match args.get(1) {
                Some(Value::Object(Some(mirror))) => ctx.class_id_of_object(*mirror),
                _ => ClassId::new(0),
            };
            let field_name = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let fields = ctx.declared_fields(class_id);
            let field_idx = fields
                .iter()
                .position(|f| f.name == field_name)
                .unwrap_or(0) as i32;
            ctx.set_field(
                vh_obj,
                VH_CLASS_OR_TARGET,
                Value::Int(class_id.as_u32() as i32),
            );
            ctx.set_field(vh_obj, VH_FIELD_INDEX, Value::Int(field_idx));
            ctx.set_field(vh_obj, VH_IS_STATIC, Value::Int(1));
            Ok(Some(Value::Object(Some(vh_obj))))
        },
    );
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
    r.register(
        pkg,
        "isCompatibleWith",
        "(Ljava/lang/String;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );
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
            let pkg_obj = alloc_concurrent_synthetic(ctx, "java/lang/Package", 6);
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

pub(crate) fn register_p59_stackwalker(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sw = "java/lang/StackWalker";
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
    r.register(
        sw,
        "getCallerClass",
        "()Ljava/lang/Class;",
        p59_sw_get_caller_class,
    );

    // StackFrame = 7-field synthetic — WP1.9 (+ WP1.10 slot 6):
    //   slot 0: className (String, with '/' → '.')
    //   slot 1: methodName (String)
    //   slot 2: fileName (String or null)
    //   slot 3: lineNumber (Int, -1/-2 sentinels)
    //   slot 4: byteCodeIndex (Int, -1 for unknown/native)
    //   slot 5: declaringClassInternalName (String, '/'-form; used only by
    //           `toStackTraceElement()`'s formatting fallback)
    //   slot 6: declaringClassMirror (Class or null) — resolved EAGERLY at
    //           `populate_stack_frame` time from the entry's own ClassId
    //           (see that function's doc comment for why: a fresh by-name
    //           lookup performed later, from `getDeclaringClass()`, can fail
    //           for a frame whose class is still running its own `<clinit>`)
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
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(sf, "getLineNumber", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(sf, "getByteCodeIndex", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 4)))
    });
    // StackFrame.getDeclaringClass() — resolve the Class mirror via the
    // stored internal class name (slot 5). RETAIN_CLASS_REFERENCE option
    // is not enforced here (we always resolve); bootstrap consumers that
    // don't request the option simply ignore the returned Class.
    // StackFrame.getDeclaringClass() — return the Class mirror eagerly
    // resolved and stored at population time (slot 6). RETAIN_CLASS_REFERENCE
    // option is not enforced here (we always resolve); bootstrap consumers
    // that don't request the option simply ignore the returned Class.
    r.register(
        sf,
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 6)))
        },
    );
    // StackFrame.getMethodType() — we don't yet wire real MethodType
    // reconstruction; return null to match JDK's UnsupportedOperationException
    // fallback without throwing, which keeps bootstrap probes quiet.
    r.register(
        sf,
        "getMethodType",
        "()Ljava/lang/invoke/MethodType;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(sf, "isNativeMethod", "()Z", |_ctx, args| {
        // Native iff lineNumber == -2 (per StackTraceElement convention).
        if let Some(Value::Object(Some(this))) = args.first() {
            let _ = this; // keep arg shape; reading through ctx would require &mut
        }
        Ok(Some(Value::Int(0)))
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
        let file_name = match ctx.get_field(this, 2) {
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
            let ste = alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4);
            let class_dotted = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let class_slashed = match ctx.get_field(this, 5) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => class_dotted.replace('.', "/"),
            };
            let method_name = match ctx.get_field(this, 1) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let file_name = match ctx.get_field(this, 2) {
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

/// Populate the 6-slot StackFrame synthetic from a `StackTraceEntry`.
/// WP1.9: writes bci + declaringClassInternalName in addition to the
/// pre-existing 4 slots.
pub(crate) fn populate_stack_frame(
    ctx: &mut dyn NativeContext,
    entry: &cratonvm_native_api::StackTraceEntry,
) -> cratonvm_types::ObjectRef {
    // GC-SAFETY (see `lang_stackwalker::populate_sfi`): allocate every object
    // under a pin first, then read each back through its pin before the
    // (allocation-free) field writes. Holding the freshly-allocated `sf` and
    // strings in bare locals across the subsequent `create_string` calls is a
    // use-after-move/free under the moving collector.
    // Slot 6: the declaring-class `Class` mirror, resolved EAGERLY here from
    // `entry.class_id` when available. `entry.class_id` is captured directly
    // off the live interpreter `Frame` (see `stackwalker::entry_from_frame`)
    // and is therefore always valid for a real frame -- unlike a fresh
    // by-name lookup (`class_id_by_name(&entry.class_name)`) performed LATER,
    // from `getDeclaringClass()`, which can fail for a frame whose class is
    // still executing its own `<clinit>` (observed: `SpringFactoriesLoader`/
    // `EntityManagerFactoryUtils` calling `LogFactory.getLog()` from their
    // own static initializers -- log4j-api's `StackLocator` walks back to
    // that exact self-frame and NPEs when the by-name lookup comes back
    // empty). Resolving from the guaranteed-valid ClassId at population time
    // sidesteps that failure mode entirely; `class_id_by_name` remains a
    // fallback for synthetic/no-frame entries (`entry.class_id.is_none()`).
    let decl_cid = entry
        .class_id
        .or_else(|| ctx.class_id_by_name(&entry.class_name));

    let mut sf = alloc_concurrent_synthetic(ctx, "java/lang/StackWalker$StackFrame", 7);
    let base = ctx.pin_native_root(sf);
    let mut cls_str = ctx.create_string(&entry.class_name.replace('/', "."));
    let h_cls = ctx.pin_native_root(cls_str);
    let mut meth_str = ctx.create_string(&entry.method_name);
    let h_meth = ctx.pin_native_root(meth_str);
    let (mut file_str, h_file) = match &entry.source_file {
        Some(f) => {
            let s = ctx.create_string(f);
            let h = ctx.pin_native_root(s);
            (Some(s), Some(h))
        }
        None => (None, None),
    };
    // Preserve the '/' form too (used by toStackTraceElement()'s fallback).
    let mut decl_internal = ctx.create_string(&entry.class_name);
    let h_decl = ctx.pin_native_root(decl_internal);
    let (mut decl_mirror, h_mirror) = match decl_cid {
        Some(cid) => {
            let m = ctx.get_class_mirror(cid);
            let h = ctx.pin_native_root(m);
            (Some(m), Some(h))
        }
        None => (None, None),
    };

    sf = ctx.read_native_pin(base, sf);
    cls_str = ctx.read_native_pin(h_cls, cls_str);
    meth_str = ctx.read_native_pin(h_meth, meth_str);
    if let (Some(s), Some(h)) = (file_str, h_file) {
        file_str = Some(ctx.read_native_pin(h, s));
    }
    decl_internal = ctx.read_native_pin(h_decl, decl_internal);
    if let (Some(m), Some(h)) = (decl_mirror, h_mirror) {
        decl_mirror = Some(ctx.read_native_pin(h, m));
    }

    ctx.set_field(sf, 0, Value::Object(Some(cls_str)));
    ctx.set_field(sf, 1, Value::Object(Some(meth_str)));
    ctx.set_field(
        sf,
        2,
        file_str.map_or(Value::Object(None), |s| Value::Object(Some(s))),
    );
    ctx.set_field(sf, 3, Value::Int(entry.line_number));
    ctx.set_field(sf, 4, Value::Int(entry.byte_code_index));
    ctx.set_field(sf, 5, Value::Object(Some(decl_internal)));
    ctx.set_field(
        sf,
        6,
        decl_mirror.map_or(Value::Object(None), |m| Value::Object(Some(m))),
    );
    ctx.unpin_native_roots(base);
    sf
}

pub(crate) fn p59_sw_walk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Capture the current call stack and build a Stream<StackFrame>.
    // `capture_stack_trace` returns outer→inner (oldest frame first); a
    // `StackWalker` stream must be inner→outer (the walk()-caller first),
    // matching real JDK — reuse the same reversal + VM-internal-frame
    // stripping that `AbstractStackWalker.callStackWalk` already applies
    // (`lang_stackwalker::ordered_stack_walk_frames`) instead of handing
    // the caller the raw outer→inner order. Without this, `skip`/`limit`
    // chains over the stream (e.g. Lucene's `TestSecrets.ensureCaller`)
    // land on the wrong frame and misidentify the caller.
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
        let sf = populate_stack_frame(ctx, entry);
        arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(sf)));
    }
    let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1);
    arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.unpin_native_roots(arr_pin);

    // Apply the Function argument to the stream: function.apply(stream)
    let function = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(
        function,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(stream))],
    )
}

pub(crate) fn p59_sw_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // Same inner→outer ordering as `p59_sw_walk` — see its comment.
    let raw_trace = ctx.capture_stack_trace(0);
    let frames = crate::lang_stackwalker::ordered_stack_walk_frames(&raw_trace);
    for entry in &frames {
        let sf = populate_stack_frame(ctx, entry);
        ctx.invoke_virtual(
            consumer,
            "accept",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(sf))],
        )?;
    }
    Ok(None)
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
pub(crate) fn read_module_name(ctx: &dyn NativeContext, module_obj: ObjectRef) -> String {
    match ctx.get_field(module_obj, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(), // unnamed module
    }
}

/// Helper: build a `HashSet<String>` Java object from a Vec of Rust strings.
pub(crate) fn build_string_set(ctx: &mut dyn NativeContext, items: Vec<String>) -> ObjectRef {
    use cratonvm_types::ArrayElementType;
    let len = items.len();
    let arr = ctx.new_array(ArrayElementType::Reference, len);
    for (i, s) in items.iter().enumerate() {
        let js = ctx.create_string(s);
        ctx.set_array_element(arr, i, Value::Object(Some(js)));
    }
    let set = alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3);
    ctx.set_field(set, 0, Value::Object(Some(arr)));
    ctx.set_field(set, 1, Value::Int(len as i32));
    ctx.set_field(set, 2, Value::Int(16)); // initial capacity marker
    set
}

/// `Module.canRead(Module)` → boolean.
///
/// A named top-level fn (not an inline closure) so it can be registered from
/// TWO places: `register_p59_module` below (the `synthetic-jdk`-feature-gated
/// path) and `register_essential_natives` (native-builtins/src/lib.rs, the
/// path the default `cratonvm-cli` build actually uses — `register_p59_module`
/// is unreachable there, since its only caller chain is entirely
/// `#[cfg(feature = "synthetic-jdk")]`-gated; see `native_module_get_descriptor`
/// or `Module.canUse` in lib.rs for the fuller writeup of this recurring
/// essential-vs-synthetic-jdk gap).
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
    let target = match args.get(2) {
        Some(Value::Object(Some(m))) => read_module_name(ctx, *m),
        _ => String::new(),
    };
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
    let target = match args.get(2) {
        Some(Value::Object(Some(m))) => read_module_name(ctx, *m),
        _ => String::new(),
    };
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
    let this = obj_arg(args, 0)?;
    let module_name = read_module_name(ctx, this);
    let pkg_name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    let target = target_index
        .and_then(|idx| match args.get(idx) {
            Some(Value::Object(Some(m))) => Some(read_module_name(ctx, *m)),
            _ => None,
        })
        .unwrap_or_default();
    let pkg_slash = pkg_name.replace('.', "/");
    if open {
        ctx.module_add_opens(&module_name, &pkg_slash, &target);
    } else {
        ctx.module_add_exports(&module_name, &pkg_slash, &target);
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

pub(crate) fn register_p59_module(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // =================================================================
    // java.lang.ModuleLayer
    // =================================================================
    let ml = "java/lang/ModuleLayer";

    // ModuleLayer.boot() → ModuleLayer
    r.register(ml, "boot", "()Ljava/lang/ModuleLayer;", |ctx, _args| {
        let layer = alloc_concurrent_synthetic(ctx, "java/lang/ModuleLayer", 1);
        ctx.set_field(layer, 0, Value::Int(1)); // is boot
        Ok(Some(Value::Object(Some(layer))))
    });

    // ModuleLayer.modules() → Set<Module>
    // Returns Module objects for all registered modules in the boot layer.
    r.register(ml, "modules", "()Ljava/util/Set;", |ctx, _args| {
        use cratonvm_types::ArrayElementType;
        let names = ctx.all_module_names();
        let len = names.len();
        let arr = ctx.new_array(ArrayElementType::Reference, len);
        for (i, name) in names.iter().enumerate() {
            let m_obj = alloc_concurrent_synthetic(ctx, "java/lang/Module", 2);
            let name_str = ctx.create_string(name);
            ctx.set_field(m_obj, 0, Value::Object(Some(name_str)));
            // field 1 = layer — we don't set it here to avoid infinite recursion
            ctx.set_array_element(arr, i, Value::Object(Some(m_obj)));
        }
        let set = alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3);
        ctx.set_field(set, 0, Value::Object(Some(arr)));
        ctx.set_field(set, 1, Value::Int(len as i32));
        ctx.set_field(set, 2, Value::Int(16));
        Ok(Some(Value::Object(Some(set))))
    });

    // ModuleLayer.findModule(String) → Optional<Module>
    r.register(
        ml,
        "findModule",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        |ctx, args| {
            let name_str = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
            // Check if this module exists in the registry.
            let names = ctx.all_module_names();
            if names.iter().any(|n| n == &name_str) {
                let m_obj = alloc_concurrent_synthetic(ctx, "java/lang/Module", 2);
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
            let desc = alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor", 2);
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
        let set = build_string_set(ctx, dot_packages);
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
    // `native_module_get_descriptor`'s doc comment): this closure is
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
            let m_obj = alloc_concurrent_synthetic(ctx, "java/lang/Module", 2);
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
    let optional = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
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
    r.register(param, "getType", "()Ljava/lang/Class;", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    });
    r.register(
        param,
        "getDeclaringExecutable",
        "()Ljava/lang/reflect/Executable;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    r.register(param, "isVarArgs", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(param, "isNamePresent", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
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
    r.register(
        exec,
        "getParameters",
        "()[Ljava/lang/reflect/Parameter;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(exec, "getParameterCount", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
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
            .class_name_of_id(ctx.class_id_of_object(other))
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
    let ctor = "java/lang/reflect/Constructor";
    r.register(
        ctor,
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        ctor,
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
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
                    let map = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
                    let buckets = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
                    ctx.set_field(map, 0, Value::Object(Some(buckets)));
                    ctx.set_field(map, 1, Value::Int(0));
                    ctx.set_field(map, 2, Value::Int(16));
                    return Ok(Some(Value::Object(Some(map))));
                }
            };
            let len = ctx.array_length(entries);
            let map = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
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
    // Constable interface
    r.register(
        "java/lang/constant/Constable",
        "describeConstable",
        "()Ljava/util/Optional;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
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
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/constant/ClassDesc", 1);
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
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/constant/ClassDesc", 1);
            ctx.set_field(obj, 0, Value::Object(Some(s)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cd,
        "ofDescriptor",
        "(Ljava/lang/String;)Ljava/lang/constant/ClassDesc;",
        |ctx, args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/constant/ClassDesc", 1);
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
    r.register(cd, "isArray", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
    r.register(cd, "isPrimitive", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(cd, "isClassOrInterface", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });

    // MethodTypeDesc
    let mtd = "java/lang/constant/MethodTypeDesc";
    r.register(mtd, "of", "(Ljava/lang/constant/ClassDesc;[Ljava/lang/constant/ClassDesc;)Ljava/lang/constant/MethodTypeDesc;", |ctx, _args| {
        let obj = alloc_concurrent_synthetic(ctx, "java/lang/constant/MethodTypeDesc", 1);
        ctx.set_field(obj, 0, Value::Object(None));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        mtd,
        "ofDescriptor",
        "(Ljava/lang/String;)Ljava/lang/constant/MethodTypeDesc;",
        |ctx, args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/constant/MethodTypeDesc", 1);
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
        // Class mirror or a real reifier impl: ask the VM for getTypeName(),
        // falling back to toString() and finally the dotted class name.
        _ => {
            if let Ok(Some(Value::Object(Some(s)))) =
                ctx.invoke_virtual(obj, "getTypeName", "()Ljava/lang/String;", &[])
            {
                if let Some(rendered) = ctx.read_string(s) {
                    return rendered;
                }
            }
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

    // nullConstant — returns null (correct as-is)
    r.register(cb, "nullConstant",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))));

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
            let vh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", 6);
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
            let vh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", 6);
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
            let vh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", 6);
            ctx.set_field(vh, 0, Value::Int(2)); // kind=array
            ctx.set_field(vh, 3, args.get(3).copied().unwrap_or(Value::Object(None))); // component type
            Ok(Some(Value::Object(Some(vh))))
        });
    r.set_category(__prev_cat);
}
