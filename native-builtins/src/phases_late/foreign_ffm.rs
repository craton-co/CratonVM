// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.lang.foreign` natives: the Foreign Function & Memory API (Arena, MemorySegment, MemoryLayout, Linker).
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// java.lang.foreign — Foreign Function & Memory API (Java 22)
// Stub implementations for Arena, MemorySegment, MemoryLayout, ValueLayout, Linker
// =============================================================================

pub(crate) fn p67_layout_object(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    byte_size: i64,
    byte_alignment: i64,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, class_name, 4);
    ctx.set_field(obj, 0, Value::Long(byte_size));
    ctx.set_field(obj, 1, Value::Long(byte_alignment));
    ctx.set_field(obj, 2, Value::Int(0));
    ctx.set_field(obj, 3, Value::Object(None));
    obj
}

pub(crate) fn p67_optional(ctx: &mut dyn NativeContext, value: Value) -> ObjectRef {
    let pinned = match value {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
    let value = match pinned {
        Some((pin, obj)) => {
            let obj = ctx.read_native_pin(pin, obj);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(obj))
        }
        None => Value::Object(None),
    };
    ctx.set_field(opt, 0, value);
    opt
}

pub(crate) fn p67_layout_name_value(ctx: &dyn NativeContext, layout: ObjectRef) -> Value {
    if matches!(ctx.get_field(layout, 0), Value::Int(_)) {
        if ctx.object_num_fields(layout) > 2 {
            ctx.get_field(layout, 2)
        } else {
            Value::Object(None)
        }
    } else if ctx.object_num_fields(layout) > 3 {
        ctx.get_field(layout, 3)
    } else {
        Value::Object(None)
    }
}

pub(crate) fn p67_layout_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = p67_layout_name_value(ctx, this);
    Ok(Some(Value::Object(Some(p67_optional(ctx, name)))))
}

pub(crate) fn p67_layout_carrier_name(class_name: &str) -> &'static str {
    if class_name == "java/lang/foreign/AddressLayout"
        || class_name.ends_with("ValueLayouts$OfAddressImpl")
    {
        "java/lang/foreign/MemorySegment"
    } else if class_name.contains("OfBoolean") {
        "boolean"
    } else if class_name.contains("OfByte") {
        "byte"
    } else if class_name.contains("OfChar") {
        "char"
    } else if class_name.contains("OfShort") {
        "short"
    } else if class_name.contains("OfInt") {
        "int"
    } else if class_name.contains("OfLong") {
        "long"
    } else if class_name.contains("OfFloat") {
        "float"
    } else if class_name.contains("OfDouble") {
        "double"
    } else {
        "java/lang/Object"
    }
}

pub(crate) fn p67_class_mirror(ctx: &mut dyn NativeContext, class_name: &str) -> ObjectRef {
    match class_name {
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void" => {
            ctx.primitive_class_mirror(class_name)
        }
        _ => {
            if let Some(cid) = ctx.class_id_by_name(class_name) {
                return ctx.get_class_mirror(cid);
            }
            if let Ok(cid) = ctx.ensure_class_initialized(class_name) {
                return ctx.get_class_mirror(cid);
            }
            alloc_concurrent_synthetic(ctx, "java/lang/Class", 2)
        }
    }
}

pub(crate) fn p67_layout_carrier(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "java/lang/foreign/ValueLayout".to_string());
    let carrier_name = p67_layout_carrier_name(&class_name);
    Ok(Some(Value::Object(Some(p67_class_mirror(
        ctx,
        carrier_name,
    )))))
}

pub(crate) fn p67_layout_with_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "java/lang/foreign/MemoryLayout".to_string());
    let field_count = ctx.object_num_fields(this);
    let name_slot = if matches!(ctx.get_field(this, 0), Value::Int(_)) {
        2
    } else {
        3
    };
    let clone_fields = std::cmp::max(field_count, name_slot + 1);
    let this_pin = ctx.pin_native_root(this);
    let name_pin = match name {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let cloned = alloc_concurrent_synthetic(ctx, &class_name, clone_fields);
    let this = ctx.read_native_pin(this_pin, this);
    for i in 0..field_count {
        ctx.set_field(cloned, i, ctx.get_field(this, i));
    }
    let name = match name_pin {
        Some((pin, obj)) => {
            let obj = ctx.read_native_pin(pin, obj);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(obj))
        }
        None => Value::Object(None),
    };
    ctx.set_field(cloned, name_slot, name);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(cloned))))
}

pub(crate) fn p67_address_layout_target_layout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let target = if ctx.object_num_fields(this) > 4 {
        ctx.get_field(this, 4)
    } else {
        Value::Object(None)
    };
    Ok(Some(Value::Object(Some(p67_optional(ctx, target)))))
}

pub(crate) fn p67_address_layout_with_target_layout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "java/lang/foreign/AddressLayout".to_string());
    let field_count = ctx.object_num_fields(this);
    let clone_fields = std::cmp::max(field_count, 5);
    let this_pin = ctx.pin_native_root(this);
    let target_pin = match target {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let cloned = alloc_concurrent_synthetic(ctx, &class_name, clone_fields);
    let this = ctx.read_native_pin(this_pin, this);
    for i in 0..field_count {
        ctx.set_field(cloned, i, ctx.get_field(this, i));
    }
    let target = match target_pin {
        Some((pin, obj)) => {
            let obj = ctx.read_native_pin(pin, obj);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(obj))
        }
        None => Value::Object(None),
    };
    ctx.set_field(cloned, 4, target);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(cloned))))
}

pub(crate) fn p67_set_value_layout_static(
    ctx: &mut dyn NativeContext,
    field_name: &str,
    class_name: &str,
    byte_size: i64,
    byte_alignment: i64,
) {
    let obj = p67_layout_object(ctx, class_name, byte_size, byte_alignment);
    ctx.set_static_field_by_name(
        "java/lang/foreign/ValueLayout",
        field_name,
        Value::Object(Some(obj)),
    );
}

pub(crate) fn p67_value_layout_clinit(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    for (field_name, class_name, byte_size, byte_alignment) in [
        ("ADDRESS", "java/lang/foreign/AddressLayout", 8_i64, 8_i64),
        (
            "JAVA_BYTE",
            "java/lang/foreign/ValueLayout$OfByte",
            1_i64,
            1_i64,
        ),
        (
            "JAVA_BOOLEAN",
            "java/lang/foreign/ValueLayout$OfBoolean",
            1_i64,
            1_i64,
        ),
        (
            "JAVA_CHAR",
            "java/lang/foreign/ValueLayout$OfChar",
            2_i64,
            2_i64,
        ),
        (
            "JAVA_SHORT",
            "java/lang/foreign/ValueLayout$OfShort",
            2_i64,
            2_i64,
        ),
        (
            "JAVA_INT",
            "java/lang/foreign/ValueLayout$OfInt",
            4_i64,
            4_i64,
        ),
        (
            "JAVA_LONG",
            "java/lang/foreign/ValueLayout$OfLong",
            8_i64,
            8_i64,
        ),
        (
            "JAVA_FLOAT",
            "java/lang/foreign/ValueLayout$OfFloat",
            4_i64,
            4_i64,
        ),
        (
            "JAVA_DOUBLE",
            "java/lang/foreign/ValueLayout$OfDouble",
            8_i64,
            8_i64,
        ),
        (
            "ADDRESS_UNALIGNED",
            "java/lang/foreign/AddressLayout",
            8_i64,
            1_i64,
        ),
        (
            "JAVA_CHAR_UNALIGNED",
            "java/lang/foreign/ValueLayout$OfChar",
            2_i64,
            1_i64,
        ),
        (
            "JAVA_SHORT_UNALIGNED",
            "java/lang/foreign/ValueLayout$OfShort",
            2_i64,
            1_i64,
        ),
        (
            "JAVA_INT_UNALIGNED",
            "java/lang/foreign/ValueLayout$OfInt",
            4_i64,
            1_i64,
        ),
        (
            "JAVA_LONG_UNALIGNED",
            "java/lang/foreign/ValueLayout$OfLong",
            8_i64,
            1_i64,
        ),
        (
            "JAVA_FLOAT_UNALIGNED",
            "java/lang/foreign/ValueLayout$OfFloat",
            4_i64,
            1_i64,
        ),
        (
            "JAVA_DOUBLE_UNALIGNED",
            "java/lang/foreign/ValueLayout$OfDouble",
            8_i64,
            1_i64,
        ),
    ] {
        p67_set_value_layout_static(ctx, field_name, class_name, byte_size, byte_alignment);
    }
    Ok(None)
}

pub(crate) fn p67_layout_byte_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, 0)))
}

pub(crate) fn p67_layout_byte_alignment(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = ctx.get_field(this, 1);
    match value {
        Value::Long(_) => Ok(Some(value)),
        _ => Ok(Some(ctx.get_field(this, 0))),
    }
}

pub(crate) fn p67_return_this(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

pub(crate) fn p67_layout_is_little(ctx: &dyn NativeContext, layout: ObjectRef) -> bool {
    if ctx.object_num_fields(layout) <= 2 {
        return true;
    }
    ctx.get_field(layout, 2)
        .as_int()
        .map(|v| v != 0)
        .unwrap_or(true)
}

pub(crate) fn p67_byte_order_object(ctx: &mut dyn NativeContext, little_endian: bool) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/nio/ByteOrder", 1);
    ctx.set_field(obj, 0, Value::Int(if little_endian { 1 } else { 0 }));
    obj
}

pub(crate) fn p67_layout_order(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let little_endian = p67_layout_is_little(ctx, this);
    Ok(Some(Value::Object(Some(p67_byte_order_object(
        ctx,
        little_endian,
    )))))
}

pub(crate) fn p67_layout_with_order(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "java/lang/foreign/ValueLayout".to_string());
    let byte_size = match ctx.get_field(this, 0) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 1,
    };
    let byte_alignment = match ctx.get_field(this, 1) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => byte_size,
    };
    let obj = p67_layout_object(ctx, &class_name, byte_size, byte_alignment);
    ctx.set_field(
        obj,
        2,
        Value::Int(if vh_byte_order_is_little(ctx, args.get(1)) {
            1
        } else {
            0
        }),
    );
    ctx.set_field(obj, 3, p67_layout_name_value(ctx, this));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn p67_memory_session(ctx: &mut dyn NativeContext) -> Value {
    let obj = alloc_concurrent_synthetic(ctx, "jdk/internal/foreign/MemorySessionImpl", 1);
    ctx.set_field(obj, 0, Value::Int(1));
    Value::Object(Some(obj))
}

pub(crate) fn p67_layout_width_obj(ctx: &dyn NativeContext, layout: ObjectRef) -> i32 {
    match ctx.get_field(layout, 0) {
        Value::Long(v) if (1..=8).contains(&v) => v as i32,
        Value::Int(_) => match ctx.get_field(layout, 1) {
            Value::Int(v) if (1..=8).contains(&v) => v,
            Value::Long(v) if (1..=8).contains(&v) => v as i32,
            _ => 1,
        },
        _ => 1,
    }
}

pub(crate) fn p67_layout_width(ctx: &dyn NativeContext, args: &[Value]) -> i32 {
    let Some(Value::Object(Some(layout))) = args.first() else {
        return 1;
    };
    p67_layout_width_obj(ctx, *layout)
}

pub(crate) fn p67_string_value(ctx: &dyn NativeContext, value: Value) -> Option<String> {
    match value {
        Value::Object(Some(obj)) => ctx.read_string(obj).filter(|s| !s.is_empty()),
        _ => None,
    }
}

pub(crate) fn p67_path_element_group_name(
    ctx: &dyn NativeContext,
    elem: ObjectRef,
) -> Option<String> {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(elem))
        .unwrap_or_default();

    if class_name == "java/lang/foreign/MemoryLayout$PathElement" {
        if ctx.get_field(elem, 1).as_int() == Some(0) {
            return p67_string_value(ctx, ctx.get_field(elem, 0));
        }
        return None;
    }

    if class_name.ends_with("LayoutPath$GroupElementByName")
        || class_name.ends_with("GroupElementByName")
    {
        return p67_string_value(ctx, ctx.get_field_by_name(elem, "name"))
            .or_else(|| p67_string_value(ctx, ctx.get_field(elem, 0)));
    }

    if let Some(name) = p67_string_value(ctx, ctx.get_field_by_name(elem, "name")) {
        return Some(name);
    }
    if ctx.get_field(elem, 1).as_int() == Some(0) {
        return p67_string_value(ctx, ctx.get_field(elem, 0));
    }
    None
}

pub(crate) fn p67_layout_named_member(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    target_name: &str,
) -> Option<ObjectRef> {
    let members = match ctx.get_field(layout, 2) {
        Value::Object(Some(arr)) => arr,
        _ => return None,
    };
    for i in 0..ctx.array_length(members) {
        let member = match ctx.get_array_element(members, i) {
            Value::Object(Some(member)) => member,
            _ => continue,
        };
        if p67_string_value(ctx, p67_layout_name_value(ctx, member)).as_deref() == Some(target_name)
        {
            return Some(member);
        }
    }
    None
}

pub(crate) fn p67_memory_layout_path_target(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    path_arr: ObjectRef,
) -> ObjectRef {
    let mut current = layout;
    for i in 0..ctx.array_length(path_arr) {
        let elem = match ctx.get_array_element(path_arr, i) {
            Value::Object(Some(elem)) => elem,
            _ => break,
        };
        let Some(name) = p67_path_element_group_name(ctx, elem) else {
            break;
        };
        let Some(member) = p67_layout_named_member(ctx, current, &name) else {
            break;
        };
        current = member;
    }
    current
}

pub(crate) fn p67_var_handle_for_layout(ctx: &mut dyn NativeContext, layout: ObjectRef) -> Value {
    let width = p67_layout_width_obj(ctx, layout);
    let little_endian = p67_layout_is_little(ctx, layout);
    let vh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS);
    ctx.set_field(
        vh,
        VH_CLASS_OR_TARGET,
        Value::Int(if little_endian { 1 } else { 0 }),
    );
    ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(width));
    ctx.set_field(vh, VH_IS_STATIC, Value::Int(VH_KIND_MEMORY_SEGMENT));
    crate::lang_invoke::register_p67_memory_segment_var_handle(ctx, vh, width);
    Value::Object(Some(vh))
}

pub(crate) fn p67_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> Value {
    match args.first() {
        Some(Value::Object(Some(layout))) => p67_var_handle_for_layout(ctx, *layout),
        _ => {
            let vh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS);
            ctx.set_field(vh, VH_CLASS_OR_TARGET, Value::Int(1));
            ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(1));
            ctx.set_field(vh, VH_IS_STATIC, Value::Int(VH_KIND_MEMORY_SEGMENT));
            Value::Object(Some(vh))
        }
    }
}

pub(crate) fn p67_memory_layout_var_handle(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let target_layout = match args.get(1) {
        Some(Value::Object(Some(path_arr))) => p67_memory_layout_path_target(ctx, this, *path_arr),
        _ => this,
    };
    Ok(Some(p67_var_handle_for_layout(ctx, target_layout)))
}

pub(crate) fn p67_segment_parts(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    width: i64,
) -> Option<(*mut u8, i64)> {
    if let Value::Long(ptr) = ctx.get_field_by_name(seg, "min") {
        let size = match ctx.get_field_by_name(seg, "length") {
            Value::Long(v) => v,
            _ => 0,
        };
        if ptr == 0 || offset < 0 || offset.saturating_add(width) > size {
            return None;
        }
        return Some((
            (ptr as usize).wrapping_add(offset as usize) as *mut u8,
            size,
        ));
    }
    if ctx.object_num_fields(seg) >= 4 {
        if let (Value::Long(size), Value::Long(ptr)) =
            (ctx.get_field(seg, 0), ctx.get_field(seg, 3))
        {
            if ptr == 0 || offset < 0 || offset.saturating_add(width) > size {
                return None;
            }
            return Some((
                (ptr as usize).wrapping_add(offset as usize) as *mut u8,
                size,
            ));
        }
    }
    if ctx.object_num_fields(seg) < 6 {
        return None;
    }
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
    if ptr == 0 || offset < 0 || offset.saturating_add(width) > size {
        return None;
    }
    let absolute_offset = base_offset.saturating_add(offset);
    Some((
        (ptr as usize).wrapping_add(absolute_offset as usize) as *mut u8,
        size,
    ))
}

pub(crate) fn p67_segment_byte_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Long(v) = ctx.get_field_by_name(this, "length") {
        return Ok(Some(Value::Long(v)));
    }
    if ctx.object_num_fields(this) >= 4 {
        if let Value::Long(v) = ctx.get_field(this, 0) {
            return Ok(Some(Value::Long(v)));
        }
    }
    if ctx.object_num_fields(this) >= 6 {
        Ok(Some(ctx.get_field(this, 1)))
    } else {
        Ok(Some(ctx.get_field(this, 0)))
    }
}

pub(crate) fn p67_segment_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Long(v) = ctx.get_field_by_name(this, "min") {
        return Ok(Some(Value::Long(v)));
    }
    if ctx.object_num_fields(this) >= 4 {
        if let Value::Long(v) = ctx.get_field(this, 3) {
            return Ok(Some(Value::Long(v)));
        }
    }
    if ctx.object_num_fields(this) >= 6 {
        let ptr = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let offset = match ctx.get_field(this, 5) {
            Value::Long(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Long(ptr + offset)))
    } else {
        Ok(Some(ctx.get_field(this, 1)))
    }
}

pub(crate) fn p67_segment_get_width(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    width: i64,
) -> MethodCallResult {
    let seg = obj_arg(args, 0)?;
    let offset = match args.get(2) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let little_endian = match args.get(1) {
        Some(Value::Object(Some(layout))) => p67_layout_is_little(ctx, *layout),
        _ => false,
    };
    let Some((addr, _size)) = p67_segment_parts(ctx, seg, offset, width) else {
        return Ok(Some(if width == 8 {
            Value::Long(0)
        } else {
            Value::Int(0)
        }));
    };
    unsafe {
        Ok(Some(match width {
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
        }))
    }
}

pub(crate) fn p67_segment_set_width(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    width: i64,
    value_arg_index: usize,
) -> MethodCallResult {
    let seg = obj_arg(args, 0)?;
    let offset = match args.get(2) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let little_endian = match args.get(1) {
        Some(Value::Object(Some(layout))) => p67_layout_is_little(ctx, *layout),
        _ => false,
    };
    let Some((addr, _size)) = p67_segment_parts(ctx, seg, offset, width) else {
        return Ok(None);
    };
    let value = args.get(value_arg_index).copied().unwrap_or(Value::Int(0));
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
    Ok(None)
}

pub(crate) fn p67_segment_copy_to_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let src = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let src_offset = match args.get(2) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let dst = obj_arg(args, 3)?;
    let dst_index = match args.get(4) {
        Some(Value::Int(v)) => *v as usize,
        Some(Value::Long(v)) => *v as usize,
        _ => 0,
    };
    let count = match args.get(5) {
        Some(Value::Int(v)) => *v as usize,
        Some(Value::Long(v)) => *v as usize,
        _ => 0,
    };
    let width = match ctx.get_field(layout, 0) {
        Value::Long(v) if (1..=8).contains(&v) => v,
        Value::Int(_) => match ctx.get_field(layout, 1) {
            Value::Int(v) if (1..=8).contains(&v) => v as i64,
            Value::Long(v) if (1..=8).contains(&v) => v,
            _ => 1,
        },
        _ => 1,
    };
    let little_endian = p67_layout_is_little(ctx, layout);
    for i in 0..count {
        let offset = src_offset + (i as i64 * width);
        let Some((addr, _size)) = p67_segment_parts(ctx, src, offset, width) else {
            break;
        };
        let value = unsafe {
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
        };
        ctx.set_array_element(dst, dst_index + i, value);
    }
    Ok(None)
}

pub(crate) fn lucene_buffered_checksum_flush(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let buffer = match ctx.get_field_by_name(this, "buffer") {
        Value::Object(Some(buffer)) => buffer,
        _ => return,
    };
    let upto = ctx
        .get_field_by_name(this, "upto")
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    if upto == 0 {
        return;
    }
    if let Value::Object(Some(checksum)) = ctx.get_field_by_name(this, "in") {
        let _ = ctx.invoke_virtual(
            checksum,
            "update",
            "([BII)V",
            &[
                Value::Object(Some(buffer)),
                Value::Int(0),
                Value::Int(upto as i32),
            ],
        );
    }
    ctx.set_field_by_name(this, "upto", Value::Int(0));
}

pub(crate) fn lucene_buffered_checksum_write(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: &[u8],
) {
    let buffer = match ctx.get_field_by_name(this, "buffer") {
        Value::Object(Some(buffer)) => buffer,
        _ => return,
    };
    let cap = ctx.array_length(buffer);
    let mut upto = ctx
        .get_field_by_name(this, "upto")
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    if upto.saturating_add(bytes.len()) > cap {
        lucene_buffered_checksum_flush(ctx, this);
        upto = ctx
            .get_field_by_name(this, "upto")
            .as_int()
            .unwrap_or(0)
            .max(0) as usize;
    }
    if upto.saturating_add(bytes.len()) > cap {
        return;
    }
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(buffer, upto + i, Value::Int(*b as i8 as i32));
    }
    ctx.set_field_by_name(this, "upto", Value::Int((upto + bytes.len()) as i32));
}

pub(crate) fn lucene_buffered_checksum_update_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    lucene_buffered_checksum_write(ctx, this, &value.to_le_bytes());
    Ok(None)
}

pub(crate) fn lucene_buffered_checksum_update_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    lucene_buffered_checksum_write(ctx, this, &value.to_le_bytes());
    Ok(None)
}

pub(crate) fn lucene_buffered_checksum_update_longs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = obj_arg(args, 1)?;
    let mut off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    for _ in 0..len {
        let value = match ctx.get_array_element(arr, off) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        lucene_buffered_checksum_write(ctx, this, &value.to_le_bytes());
        off += 1;
    }
    Ok(None)
}

pub(crate) fn lucene_buffered_checksum_index_input_get_checksum(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Mirror the real Lucene bytecode exactly: `return digest.getValue();`.
    //
    // The previous implementation, whenever the input's read position was
    // within 8 bytes of EOF, RE-READ the file and recomputed a CRC over
    // `length - 8` bytes (a workaround for a since-fixed broken digest
    // path, shaped around CodecUtil's footer idiom where getChecksum() is
    // called at exactly length-8). That heuristic returned the WRONG value
    // for every other caller shape — e.g. a caller that reads the entire
    // file through openChecksumInput() got CRC(file[0..len-8]) instead of
    // CRC(everything read), diverging from HotSpot on identical bytes
    // (docs/internal/fixed-suite-bugs/s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md
    // item 4, ProbeNIOFS2: 170114997 vs 2329538857) — and silently re-read
    // the whole file on every near-EOF getChecksum() call. The digest path
    // (BufferedChecksum over java.util.zip.CRC32) is verified correct, so
    // just return it.
    let this = obj_arg(args, 0)?;
    match ctx.get_field_by_name(this, "digest") {
        Value::Object(Some(digest)) => ctx.invoke_virtual(digest, "getValue", "()J", &[]),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub(crate) fn register_p67_foreign_memory(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "org/apache/lucene/store/BufferedChecksumIndexInput",
        "getChecksum",
        "()J",
        lucene_buffered_checksum_index_input_get_checksum,
    );
    r.register(
        "org/apache/lucene/store/BufferedChecksum",
        "updateInt",
        "(I)V",
        lucene_buffered_checksum_update_int,
    );
    r.register(
        "org/apache/lucene/store/BufferedChecksum",
        "updateLong",
        "(J)V",
        lucene_buffered_checksum_update_long,
    );
    r.register(
        "org/apache/lucene/store/BufferedChecksum",
        "updateLongs",
        "([JII)V",
        lucene_buffered_checksum_update_longs,
    );
    // Arena = 1-field (open=0 Int)
    let arena = "java/lang/foreign/Arena";
    r.register(
        arena,
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 1);
            ctx.set_field(obj, 0, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        arena,
        "ofAuto",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 1);
            ctx.set_field(obj, 0, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        arena,
        "ofShared",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 1);
            ctx.set_field(obj, 0, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        arena,
        "global",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 1);
            ctx.set_field(obj, 0, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        arena,
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let size = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 2);
            ctx.set_field(seg, 0, Value::Long(size));
            ctx.set_field(seg, 1, Value::Long(0)); // address
            Ok(Some(Value::Object(Some(seg))))
        },
    );
    r.register(
        arena,
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let size = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 2);
            ctx.set_field(seg, 0, Value::Long(size));
            ctx.set_field(seg, 1, Value::Long(0));
            Ok(Some(Value::Object(Some(seg))))
        },
    );
    r.register(
        arena,
        "allocateFrom",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let len = match args.get(1) {
                Some(Value::Object(Some(s))) => {
                    ctx.read_string(*s).map(|t| t.len() as i64 + 1).unwrap_or(1)
                }
                _ => 1,
            };
            let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 2);
            ctx.set_field(seg, 0, Value::Long(len));
            ctx.set_field(seg, 1, Value::Long(0));
            Ok(Some(Value::Object(Some(seg))))
        },
    );
    r.register(arena, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(0));
        Ok(None)
    });
    r.register(
        arena,
        "scope",
        "()Ljava/lang/foreign/MemorySegment$Scope;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx))),
    );
    let session = "jdk/internal/foreign/MemorySessionImpl";
    r.register(
        session,
        "toMemorySession",
        "(Ljava/lang/foreign/Arena;)Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx))),
    );
    r.register(
        session,
        "createConfined",
        "(Ljava/lang/Thread;)Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx))),
    );
    r.register(
        session,
        "createShared",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx))),
    );
    r.register(
        session,
        "createImplicit",
        "(Ljava/lang/ref/Cleaner;)Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx))),
    );
    r.register(
        session,
        "createHeap",
        "(Ljava/lang/Object;)Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx))),
    );
    r.register(
        session,
        "addCloseAction",
        "(Ljava/lang/Runnable;)V",
        native_noop_with_this,
    );
    r.register(
        session,
        "addOrCleanupIfFail",
        "(Ljdk/internal/foreign/MemorySessionImpl$ResourceList$ResourceCleanup;)V",
        native_noop_with_this,
    );
    r.register(
        session,
        "addInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl$ResourceList$ResourceCleanup;)V",
        native_noop_with_this,
    );
    r.register(session, "release0", "()V", native_noop_with_this);
    r.register(session, "acquire0", "()V", native_noop_with_this);
    r.register(
        session,
        "whileAlive",
        "(Ljava/lang/Runnable;)V",
        native_noop_with_this,
    );
    r.register(
        session,
        "ownerThread",
        "()Ljava/lang/Thread;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.current_thread_object())))),
    );
    r.register(
        session,
        "isAccessibleBy",
        "(Ljava/lang/Thread;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );
    r.register(session, "isAlive", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(session, "checkValidStateRaw", "()V", native_noop_with_this);
    r.register(session, "checkValidState", "()V", native_noop_with_this);
    r.register(
        session,
        "checkValidState",
        "(Ljava/lang/foreign/MemorySegment;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(session, "isCloseable", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(session, "close", "()V", native_noop_with_this);
    r.register(session, "justClose", "()V", native_noop_with_this);

    // MemorySegment = 2-field (byteSize=0 Long, address=1 Long)
    let ms = "java/lang/foreign/MemorySegment";
    r.register(ms, "byteSize", "()J", p67_segment_byte_size);
    r.register(ms, "address", "()J", p67_segment_address);
    r.register(
        ms,
        "copy",
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;II)V",
        p67_segment_copy_to_array,
    );
    r.register(
        ms,
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
        |ctx, args| p67_segment_get_width(ctx, args, 1),
    );
    r.register(
        ms,
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
        |ctx, args| p67_segment_get_width(ctx, args, 2),
    );
    r.register(
        ms,
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
        |ctx, args| p67_segment_get_width(ctx, args, 4),
    );
    r.register(
        ms,
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
        |ctx, args| p67_segment_get_width(ctx, args, 8),
    );
    r.register(
        ms,
        "set",
        "(Ljava/lang/foreign/ValueLayout$OfByte;JB)V",
        |ctx, args| p67_segment_set_width(ctx, args, 1, 3),
    );
    r.register(
        ms,
        "set",
        "(Ljava/lang/foreign/ValueLayout$OfShort;JS)V",
        |ctx, args| p67_segment_set_width(ctx, args, 2, 3),
    );
    r.register(
        ms,
        "set",
        "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V",
        |ctx, args| p67_segment_set_width(ctx, args, 4, 3),
    );
    r.register(
        ms,
        "set",
        "(Ljava/lang/foreign/ValueLayout$OfLong;JJ)V",
        |ctx, args| p67_segment_set_width(ctx, args, 8, 3),
    );
    r.register(
        ms,
        "asSlice",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let size = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            if ctx.object_num_fields(this) >= 6 {
                let base_ptr = match ctx.get_field(this, 0) {
                    Value::Long(v) => v,
                    _ => 0,
                };
                let base_off = match ctx.get_field(this, 5) {
                    Value::Long(v) => v,
                    _ => 0,
                };
                let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
                ctx.set_field(seg, 0, Value::Long(base_ptr));
                ctx.set_field(seg, 1, Value::Long(size));
                ctx.set_field(seg, 2, ctx.get_field(this, 2));
                ctx.set_field(seg, 3, ctx.get_field(this, 3));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(base_off + offset));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 2);
                ctx.set_field(seg, 0, Value::Long(size));
                ctx.set_field(seg, 1, Value::Long(offset));
                Ok(Some(Value::Object(Some(seg))))
            }
        },
    );
    r.register(ms, "isNative", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
    r.register(ms, "isMapped", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
    r.register(ms, "isReadOnly", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(ms, "equals", "(Ljava/lang/Object;)Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(
        ms,
        "scope",
        "()Ljava/lang/foreign/MemorySegment$Scope;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx))),
    );
    r.register(
        ms,
        "NULL",
        "Ljava/lang/foreign/MemorySegment;",
        |ctx, _args| {
            let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 2);
            ctx.set_field(seg, 0, Value::Long(0));
            ctx.set_field(seg, 1, Value::Long(0));
            Ok(Some(Value::Object(Some(seg))))
        },
    );
    for ms_impl in [
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "jdk/internal/foreign/MappedMemorySegmentImpl",
    ] {
        r.register(ms_impl, "byteSize", "()J", p67_segment_byte_size);
        r.register(ms_impl, "address", "()J", p67_segment_address);
        r.register(
            ms_impl,
            "get",
            "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
            |ctx, args| p67_segment_get_width(ctx, args, 1),
        );
        r.register(
            ms_impl,
            "get",
            "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
            |ctx, args| p67_segment_get_width(ctx, args, 2),
        );
        r.register(
            ms_impl,
            "get",
            "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
            |ctx, args| p67_segment_get_width(ctx, args, 4),
        );
        r.register(
            ms_impl,
            "get",
            "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
            |ctx, args| p67_segment_get_width(ctx, args, 8),
        );
        r.register(ms_impl, "isNative", "()Z", |_ctx, _args| {
            Ok(Some(Value::Int(1)))
        });
        r.register(ms_impl, "isMapped", "()Z", |_ctx, _args| {
            Ok(Some(Value::Int(1)))
        });
        r.register(ms_impl, "isReadOnly", "()Z", |_ctx, _args| {
            Ok(Some(Value::Int(0)))
        });
        r.register(
            ms_impl,
            "scope",
            "()Ljava/lang/foreign/MemorySegment$Scope;",
            |ctx, _args| Ok(Some(p67_memory_session(ctx))),
        );
    }

    // ValueLayout constants
    let vl = "java/lang/foreign/ValueLayout";
    r.register(vl, "<clinit>", "()V", p67_value_layout_clinit);
    r.register(
        vl,
        "JAVA_BYTE",
        "Ljava/lang/foreign/ValueLayout$OfByte;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfByte", 1, 1);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_BOOLEAN",
        "Ljava/lang/foreign/ValueLayout$OfBoolean;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfBoolean", 1, 1);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_CHAR",
        "Ljava/lang/foreign/ValueLayout$OfChar;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfChar", 2, 2);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_SHORT",
        "Ljava/lang/foreign/ValueLayout$OfShort;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfShort", 2, 2);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_INT",
        "Ljava/lang/foreign/ValueLayout$OfInt;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfInt", 4, 4);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_LONG",
        "Ljava/lang/foreign/ValueLayout$OfLong;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfLong", 8, 8);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_FLOAT",
        "Ljava/lang/foreign/ValueLayout$OfFloat;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfFloat", 4, 4);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_DOUBLE",
        "Ljava/lang/foreign/ValueLayout$OfDouble;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfDouble", 8, 8);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "ADDRESS",
        "Ljava/lang/foreign/AddressLayout;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/AddressLayout", 8, 8);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // ValueLayout.OfByte/OfInt/OfLong — byteSize
    for class in [
        "java/lang/foreign/ValueLayout$OfByte",
        "java/lang/foreign/ValueLayout$OfBoolean",
        "java/lang/foreign/ValueLayout$OfChar",
        "java/lang/foreign/ValueLayout$OfShort",
        "java/lang/foreign/ValueLayout$OfInt",
        "java/lang/foreign/ValueLayout$OfLong",
        "java/lang/foreign/ValueLayout$OfFloat",
        "java/lang/foreign/ValueLayout$OfDouble",
        "java/lang/foreign/AddressLayout",
    ] {
        r.register(class, "byteSize", "()J", p67_layout_byte_size);
        r.register(class, "byteAlignment", "()J", p67_layout_byte_alignment);
        r.register(class, "carrier", "()Ljava/lang/Class;", p67_layout_carrier);
        r.register(class, "order", "()Ljava/nio/ByteOrder;", p67_layout_order);
        r.register(
            class,
            "varHandle",
            "()Ljava/lang/invoke/VarHandle;",
            |ctx, args| Ok(Some(p67_var_handle(ctx, args))),
        );
    }
    for (class, specific_desc) in [
        (
            "java/lang/foreign/ValueLayout$OfByte",
            "Ljava/lang/foreign/ValueLayout$OfByte;",
        ),
        (
            "java/lang/foreign/ValueLayout$OfBoolean",
            "Ljava/lang/foreign/ValueLayout$OfBoolean;",
        ),
        (
            "java/lang/foreign/ValueLayout$OfChar",
            "Ljava/lang/foreign/ValueLayout$OfChar;",
        ),
        (
            "java/lang/foreign/ValueLayout$OfShort",
            "Ljava/lang/foreign/ValueLayout$OfShort;",
        ),
        (
            "java/lang/foreign/ValueLayout$OfInt",
            "Ljava/lang/foreign/ValueLayout$OfInt;",
        ),
        (
            "java/lang/foreign/ValueLayout$OfLong",
            "Ljava/lang/foreign/ValueLayout$OfLong;",
        ),
        (
            "java/lang/foreign/ValueLayout$OfFloat",
            "Ljava/lang/foreign/ValueLayout$OfFloat;",
        ),
        (
            "java/lang/foreign/ValueLayout$OfDouble",
            "Ljava/lang/foreign/ValueLayout$OfDouble;",
        ),
    ] {
        let with_alignment_specific = format!("(J){specific_desc}");
        r.register(
            class,
            "withByteAlignment",
            &with_alignment_specific,
            p67_return_this,
        );
        let with_name_specific = format!("(Ljava/lang/String;){specific_desc}");
        r.register(class, "withName", &with_name_specific, p67_layout_with_name);
        let with_order_specific = format!("(Ljava/nio/ByteOrder;){specific_desc}");
        r.register(
            class,
            "withOrder",
            &with_order_specific,
            p67_layout_with_order,
        );
        r.register(
            class,
            "withByteAlignment",
            "(J)Ljava/lang/foreign/MemoryLayout;",
            p67_return_this,
        );
        r.register(
            class,
            "withByteAlignment",
            "(J)Ljava/lang/foreign/ValueLayout;",
            p67_return_this,
        );
        r.register(
            class,
            "withName",
            "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
            p67_layout_with_name,
        );
        r.register(
            class,
            "withName",
            "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
            p67_layout_with_name,
        );
        r.register(
            class,
            "withOrder",
            "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
            p67_layout_with_order,
        );
        r.register(class, "name", "()Ljava/util/Optional;", p67_layout_name);
        r.register(class, "carrier", "()Ljava/lang/Class;", p67_layout_carrier);
        r.register(class, "order", "()Ljava/nio/ByteOrder;", p67_layout_order);
    }
    r.register(
        "java/lang/foreign/AddressLayout",
        "withTargetLayout",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/AddressLayout;",
        p67_address_layout_with_target_layout,
    );
    r.register(
        "java/lang/foreign/AddressLayout",
        "targetLayout",
        "()Ljava/util/Optional;",
        p67_address_layout_target_layout,
    );
    r.register(
        "java/lang/foreign/MemoryLayout",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        "java/lang/foreign/ValueLayout",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        "java/lang/foreign/ValueLayout",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
        p67_layout_with_name,
    );
    r.register(
        "java/lang/foreign/ValueLayout",
        "carrier",
        "()Ljava/lang/Class;",
        p67_layout_carrier,
    );
    r.register(
        "java/lang/foreign/ValueLayout",
        "order",
        "()Ljava/nio/ByteOrder;",
        p67_layout_order,
    );
    r.register(
        "java/lang/foreign/AddressLayout",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/AddressLayout;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        "java/lang/foreign/AddressLayout",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/AddressLayout;",
        p67_layout_with_name,
    );
    r.register(
        "java/lang/foreign/AddressLayout",
        "carrier",
        "()Ljava/lang/Class;",
        p67_layout_carrier,
    );
    r.register(
        "java/lang/foreign/AddressLayout",
        "order",
        "()Ljava/nio/ByteOrder;",
        p67_layout_order,
    );
    for (class, specific_desc) in [
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
            "Ljava/lang/foreign/AddressLayout;",
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
            "Ljava/lang/foreign/ValueLayout$OfByte;",
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
            "Ljava/lang/foreign/ValueLayout$OfBoolean;",
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
            "Ljava/lang/foreign/ValueLayout$OfChar;",
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
            "Ljava/lang/foreign/ValueLayout$OfShort;",
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            "Ljava/lang/foreign/ValueLayout$OfInt;",
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
            "Ljava/lang/foreign/ValueLayout$OfLong;",
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
            "Ljava/lang/foreign/ValueLayout$OfFloat;",
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
            "Ljava/lang/foreign/ValueLayout$OfDouble;",
        ),
    ] {
        let byte_alignment_specific = format!("(J){specific_desc}");
        r.register(
            class,
            "withByteAlignment",
            &byte_alignment_specific,
            |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
        );
        let with_name_specific = format!("(Ljava/lang/String;){specific_desc}");
        r.register(class, "withName", &with_name_specific, p67_layout_with_name);
        let with_order_specific = format!("(Ljava/nio/ByteOrder;){specific_desc}");
        r.register(
            class,
            "withOrder",
            &with_order_specific,
            p67_layout_with_order,
        );
        r.register(
            class,
            "withByteAlignment",
            "(J)Ljava/lang/foreign/MemoryLayout;",
            |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
        );
        r.register(
            class,
            "withByteAlignment",
            "(J)Ljava/lang/foreign/ValueLayout;",
            |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
        );
        r.register(
            class,
            "withName",
            "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
            p67_layout_with_name,
        );
        r.register(
            class,
            "withName",
            "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
            p67_layout_with_name,
        );
        r.register(
            class,
            "withOrder",
            "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
            p67_layout_with_order,
        );
        r.register(class, "name", "()Ljava/util/Optional;", p67_layout_name);
        r.register(class, "carrier", "()Ljava/lang/Class;", p67_layout_carrier);
        r.register(class, "order", "()Ljava/nio/ByteOrder;", p67_layout_order);
    }
    r.register(
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withTargetLayout",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/AddressLayout;",
        p67_address_layout_with_target_layout,
    );
    r.register(
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "targetLayout",
        "()Ljava/util/Optional;",
        p67_address_layout_target_layout,
    );

    // MemoryLayout
    let ml = "java/lang/foreign/MemoryLayout";
    r.register(
        ml,
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
        |ctx, args| {
            let members = match args.first() {
                Some(Value::Object(Some(arr))) => *arr,
                _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
            };
            let count = ctx.array_length(members);
            let mut offset = 0_i64;
            let mut max_align = 1_i64;
            for i in 0..count {
                let Some(member) = (match ctx.get_array_element(members, i) {
                    Value::Object(Some(obj)) => Some(obj),
                    _ => None,
                }) else {
                    continue;
                };
                let size = match ctx.get_field(member, 0) {
                    Value::Long(v) => v,
                    _ => match ctx.get_field(member, 1) {
                        Value::Int(v) => v as i64,
                        Value::Long(v) => v,
                        _ => 0,
                    },
                };
                let align = match ctx.get_field(member, 1) {
                    Value::Long(v) if v > 0 => v,
                    Value::Int(v) if v > 0 => v as i64,
                    _ => size.max(1),
                };
                offset = ((offset + align - 1) / align) * align;
                offset = offset.saturating_add(size.max(0));
                max_align = max_align.max(align);
            }
            let total_size = ((offset + max_align - 1) / max_align) * max_align;
            let members_pin = ctx.pin_native_root(members);
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/StructLayout", 4);
            let members = ctx.read_native_pin(members_pin, members);
            ctx.set_field(obj, 0, Value::Long(total_size));
            ctx.set_field(obj, 1, Value::Long(max_align));
            ctx.set_field(obj, 2, Value::Object(Some(members)));
            ctx.set_field(obj, 3, Value::Object(None));
            ctx.unpin_native_roots(members_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ml,
        "sequenceLayout",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/SequenceLayout;",
        |ctx, args| {
            let count = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/SequenceLayout", 1);
            ctx.set_field(obj, 0, Value::Long(count));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ml,
        "unionLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/UnionLayout", 1);
            ctx.set_field(obj, 0, Value::Long(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ml,
        "paddingLayout",
        "(J)Ljava/lang/foreign/PaddingLayout;",
        |ctx, args| {
            let size = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/PaddingLayout", 1);
            ctx.set_field(obj, 0, Value::Long(size));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ml,
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
        p67_layout_with_name,
    );
    r.register(
        ml,
        "varHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
        p67_memory_layout_var_handle,
    );
    r.register(ml, "name", "()Ljava/util/Optional;", p67_layout_name);

    // Linker
    let gl = "java/lang/foreign/GroupLayout";
    for layout_class in [gl, "java/lang/foreign/StructLayout"] {
        r.register(
            layout_class,
            "memberLayouts",
            "()Ljava/util/List;",
            |ctx, args| {
                let members =
                    match obj_arg(args, 0)
                        .ok()
                        .and_then(|this| match ctx.get_field(this, 2) {
                            Value::Object(Some(arr)) => Some(arr),
                            _ => None,
                        }) {
                        Some(arr) => arr,
                        None => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
                    };
                let len = ctx.array_length(members);
                let members_pin = ctx.pin_native_root(members);
                let data_slot = ctx
                    .resolve_field_index("java/util/ArrayList", "elementData")
                    .unwrap_or(0);
                let size_slot = ctx
                    .resolve_field_index("java/util/ArrayList", "size")
                    .unwrap_or(1);
                let n_fields = std::cmp::max(data_slot, size_slot) + 1;
                let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", n_fields);
                let members = ctx.read_native_pin(members_pin, members);
                ctx.set_field(list, data_slot, Value::Object(Some(members)));
                ctx.set_field(list, size_slot, Value::Int(len as i32));
                ctx.unpin_native_roots(members_pin);
                Ok(Some(Value::Object(Some(list))))
            },
        );
        r.register(
            layout_class,
            "name",
            "()Ljava/util/Optional;",
            p67_layout_name,
        );
        r.register(
            layout_class,
            "withName",
            "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
            p67_layout_with_name,
        );
        r.register(layout_class, "byteSize", "()J", p67_layout_byte_size);
        r.register(
            layout_class,
            "byteAlignment",
            "()J",
            p67_layout_byte_alignment,
        );
    }

    let linker = "java/lang/foreign/Linker";

    // Real JDK Arena.ofAuto returns ArenaImpl; its concrete allocate(long,
    // long) must be intercepted so default SegmentAllocator.allocate(layout)
    // produces our validated native MemorySegment representation.
    r.register(
        "jdk/internal/foreign/ArenaImpl",
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        crate::panama::pe_arena_allocate,
    );
    r.register(
        "java/lang/foreign/Arena",
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        crate::panama::pe_arena_allocate,
    );
    // Arena.allocate(long) is an interface default method in the real JDK.
    // Route it directly so it cannot construct a JDK segment whose layout
    // differs from the native MemorySegment bridge.
    r.register(
        "java/lang/foreign/Arena",
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        crate::panama::pe_arena_allocate,
    );
    r.register(
        "java/lang/foreign/MemorySegment",
        "reinterpret",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let size = match args.get(1) {
                Some(Value::Long(size)) => *size,
                _ => 0,
            };
            let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
            ctx.set_field(seg, 0, ctx.get_field(this, 0));
            ctx.set_field(seg, 1, Value::Long(size));
            ctx.set_field(seg, 2, ctx.get_field(this, 2));
            ctx.set_field(seg, 3, ctx.get_field(this, 3));
            ctx.set_field(seg, 4, Value::Int(1));
            ctx.set_field(seg, 5, ctx.get_field(this, 5));
            Ok(Some(Value::Object(Some(seg))))
        },
    );
    r.register(
        "java/lang/foreign/MemorySegment",
        "getUtf8String",
        "(J)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = match args.get(1) {
                Some(Value::Long(offset)) if *offset >= 0 => *offset,
                _ => 0,
            };
            let base = match ctx.get_field(this, 0) {
                Value::Long(address) => address,
                _ => 0,
            };
            let base_offset = match ctx.get_field(this, 5) {
                Value::Long(offset) => offset,
                _ => 0,
            };
            let remaining = match ctx.get_field(this, 1) {
                Value::Long(size) if size > offset => size - offset,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "getUtf8String requires a non-empty reinterpreted MemorySegment"
                            .into(),
                    }
                    .into());
                }
            };
            let address = (base as u64)
                .checked_add(base_offset as u64)
                .and_then(|address| address.checked_add(offset as u64))
                .ok_or_else(|| -> MethodCallFailed {
                    RuntimeError::IllegalStateException {
                        message: "getUtf8String address arithmetic overflow".into(),
                    }
                    .into()
                })? as *const u8;
            if address.is_null() {
                return Ok(Some(Value::Object(None)));
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(address, (remaining as usize).min(4096)) };
            let nul =
                bytes
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or_else(|| -> MethodCallFailed {
                        RuntimeError::IllegalStateException {
                            message: "getUtf8String exceeded its bounded scan".into(),
                        }
                        .into()
                    })?;
            let text = std::str::from_utf8(&bytes[..nul]).unwrap_or("");
            Ok(Some(Value::Object(Some(ctx.create_string(text)))))
        },
    );

    r.register(
        linker,
        "nativeLinker",
        "()Ljava/lang/foreign/Linker;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        linker,
        "defaultLookup",
        "()Ljava/lang/foreign/SymbolLookup;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 2);
            ctx.set_field(obj, 0, Value::Long(-1)); // -1 = default/system lookup
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        linker,
        "downcallHandle",
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let addr_seg = obj_arg(args, 1)?;
            let descriptor = obj_arg(args, 2)?;
            let fn_addr = match ctx.get_field(addr_seg, 0) {
                Value::Long(v) => v,
                _ => 0,
            };

            let mut variadic_fixed: i64 = -1;
            let mut capture_call_state = false;
            if let Some(Value::Object(Some(opts))) = args.get(3) {
                let n = ctx.array_length(*opts);
                for i in 0..n {
                    if let Value::Object(Some(opt)) = ctx.get_array_element(*opts, i) {
                        if crate::panama::downcall_option_captures_call_state(ctx, opt) {
                            capture_call_state = true;
                        }
                        let kind = match ctx.get_field(opt, 0) {
                            Value::Int(k) => k,
                            _ => -1,
                        };
                        if kind == 0 {
                            variadic_fixed = match ctx.get_field(opt, 1) {
                                Value::Long(v) => v,
                                Value::Int(v) => v as i64,
                                _ => -1,
                            };
                        }
                    }
                }
            }

            if std::env::var_os("CRATONVM_DBG_LINKER").is_some() {
                eprintln!(
                    "[LATE_LINKER] option downcall addr=0x{fn_addr:x} options={}",
                    args.get(3).is_some()
                );
            }
            let dh = alloc_concurrent_synthetic(ctx, "java/lang/foreign/DowncallHandle", 5);
            ctx.set_field(dh, 0, Value::Long(fn_addr));
            ctx.set_field(dh, 1, Value::Object(Some(descriptor)));
            ctx.set_field(dh, 2, Value::Long(variadic_fixed));
            ctx.set_field(dh, 3, Value::Long(0)); // cif cache not yet built
            ctx.set_field(dh, 4, Value::Int(capture_call_state as i32));
            Ok(Some(Value::Object(Some(dh))))
        }
    );
    let dh = "java/lang/foreign/DowncallHandle";
    r.register(
        dh,
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        crate::panama::pe_downcall_invoke,
    );
    r.register(
        dh,
        "invokeExact",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        crate::panama::pe_downcall_invoke,
    );
    r.register(
        dh,
        "invokeBasic",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        crate::panama::pe_downcall_invoke,
    );
    r.register(
        dh,
        "type",
        "()Ljava/lang/invoke/MethodType;",
        crate::panama::pe_downcall_type,
    );

    // FunctionDescriptor
    let fd = "java/lang/foreign/FunctionDescriptor";
    r.register(
        fd,
        "of",
        "(Ljava/lang/foreign/MemoryLayout;[Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;",
        |ctx, args| {
            let return_layout = obj_arg(args, 0)?;
            let params = match args.get(1) {
                Some(Value::Object(Some(arr))) => *arr,
                _ => ctx.new_array(ArrayElementType::Reference, 0),
            };
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2);
            ctx.set_field(obj, 0, Value::Object(Some(return_layout)));
            ctx.set_field(obj, 1, Value::Object(Some(params)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        fd,
        "ofVoid",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;",
        |ctx, args| {
            let params = match args.first() {
                Some(Value::Object(Some(arr))) => *arr,
                _ => ctx.new_array(ArrayElementType::Reference, 0),
            };
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2);
            ctx.set_field(obj, 0, Value::Object(None));
            ctx.set_field(obj, 1, Value::Object(Some(params)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // SymbolLookup — `loaderLookup`/`libraryLookup`/`find` are registered by
    // `panama::register_pe_symbol_lookup` (promoted to `Bridge` category
    // there specifically so it survives strict-no-stubs dropping). That
    // implementation actually attempts a real library load via
    // `ctx.load_native_library` and wraps results in a genuine
    // `Optional`/`Optional.empty()` rather than a bare Java `null`.
    //
    // A duplicate, unconditionally-"successful" stub trio used to live here
    // too (`libraryLookup` always allocating a fake lookup regardless of
    // whether any library was found, `find` always returning raw `null`).
    // Because this function runs under the always-on `Bridge` category while
    // `register_pe_symbol_lookup` ran under the default `SyntheticStub`
    // category (silently dropped under strict-no-stubs), THIS stub trio was
    // the one actually winning the `(class, method, descriptor)` registry
    // key — see `native_method_hash`/`self.methods.insert` last-registration-
    // wins semantics. Real JDK bytecode composes lookups via
    // `SymbolLookup.or()`, whose generated lambda does
    // `this.find(name).or(() -> other.find(name))` — a bare `null` receiver
    // there throws `NullPointerException: Cannot invoke
    // "java.util.Optional.or(java.util.function.Supplier)"` instead of
    // letting the composed lookup gracefully report "symbol not found".
    // Tomcat's `openssl_h` (jextract FFM bindings) hits exactly this in its
    // `<clinit>` when OpenSSL isn't installed, on a path Linux exercises
    // identically but this stub trio's placement made real bytecode's own
    // `.or()` compose over a lie ("yes, a library IS loaded") instead of a
    // clean unavailable signal.
    r.set_category(__prev_cat);
}
