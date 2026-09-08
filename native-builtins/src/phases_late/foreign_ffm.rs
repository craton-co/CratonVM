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
) -> Result<ObjectRef, MethodCallFailed> {
    // The REAL class where there is one. `try_alloc_concurrent_synthetic` takes
    // the larger of this request and the loaded class's own field count, so the
    // 4 is a floor and a real `OfIntImpl` still gets its six.
    let minted = p67_value_layout_impl(class_name).unwrap_or(class_name);
    let obj = try_alloc_concurrent_synthetic(ctx, minted, 4)?;
    let slots = p67_layout_slots_for_mint(ctx, obj, 3);
    ctx.set_field(obj, slots.byte_size, Value::Long(byte_size));
    ctx.set_field(obj, slots.byte_alignment, Value::Long(byte_alignment));
    // Slot 2 is the little-endian flag, and every `java.lang.foreign` layout
    // this factory stands in for is built by the JDK from
    // `ByteOrder.nativeOrder()`. It used to be a hard-coded `Int(0)` — "big
    // endian" — for every layout on every host, and nothing noticed because
    // `p67_layout_is_little` short-circuited on a FIELD COUNT: the FFM preseed
    // hands out two-slot objects, and `object_num_fields <= 2` answered
    // "little" before the flag was ever read. Measured 2026-08-10 by
    // `probes/W2ValueLayoutProbe` the moment that preseed stopped running:
    // every constant reported `order() == BIG_ENDIAN` while
    // `ByteOrder.nativeOrder()` two lines above answered LITTLE_ENDIAN.
    ctx.set_field(obj, slots.name, Value::Object(None));
    // LAST, because the real arm allocates and can move `obj`.
    let obj = p67_layout_set_order(ctx, obj, cfg!(target_endian = "little"))?;
    Ok(obj)
}

pub(crate) fn p67_optional(
    ctx: &mut dyn NativeContext,
    value: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    let pinned = match value {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
    let value = match pinned {
        Some((pin, obj)) => {
            let obj = ctx.read_native_pin(pin, obj);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(obj))
        }
        None => Value::Object(None),
    };
    ctx.set_field(opt, 0, value);
    Ok(opt)
}

/// Where a layout carrier keeps the three fields EVERY `MemoryLayout` has.
///
/// `jdk.internal.foreign.layout.AbstractLayout` declares `byteSize`,
/// `byteAlignment` and `name`, and every real layout class extends it -- value,
/// struct, sequence, union, padding. So one resolver serves all of them, which
/// is what makes moving thirteen carriers onto their real classes a change to
/// the mint sites rather than to every reader.
///
/// The carriers this file fabricates declare no fields at all, and for those the
/// map is the one `p67_layout_object` writes. Asked by NAME, never by width or
/// by the type that happens to be in a slot: guessing the shape is what made
/// `Arena.allocate` throw `Arena is closed` on 199 of 199 rows when the arena
/// carrier's width changed.
pub(crate) struct P67LayoutSlots {
    pub(crate) byte_size: usize,
    pub(crate) byte_alignment: usize,
    pub(crate) name: usize,
    pub(crate) order: P67LayoutOrder,
    /// Where an address layout keeps its target. `None` when the class declares
    /// no such field AND the carrier is too narrow to have one.
    pub(crate) target: usize,
    /// The group carriers' payload: a struct's or union's member array
    /// (`elements` on the real `AbstractGroupLayout`), or a sequence's
    /// `elementLayout`. A class has at most one of the two, so one slot serves
    /// both and the fabricated carriers keep it at 2.
    pub(crate) payload: usize,
    /// A sequence's stored element count (`elemCount`), 4 on the fabricated
    /// carrier.
    pub(crate) element_count: usize,
}

/// How a carrier records byte order -- the one place the two conventions are
/// genuinely different rather than merely differently indexed.
pub(crate) enum P67LayoutOrder {
    /// A real value layout declares `order`, a `java.nio.ByteOrder` REFERENCE.
    Object(usize),
    /// A fabricated carrier keeps a bare little-endian flag instead.
    Flag(usize),
}

/// [`p67_layout_slots`] for a carrier that is still being FILLED IN.
///
/// The fallback map's `name` index cannot be derived from slot 0 here, because
/// slot 0 has not been written yet and an unwritten slot reads back as
/// `Int(0)` — which is exactly the `Int` the derivation looks for. Every mint in
/// this file writes the four-or-five slot map with `name` last, so they pass 3;
/// asking the half-built object instead put `name` on top of the member array
/// and cost `byteOffset(groupElement(..))` its members.
pub(crate) fn p67_layout_slots_for_mint(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    fabricated_name: usize,
) -> P67LayoutSlots {
    p67_layout_slots_inner(ctx, layout, Some(fabricated_name))
}

pub(crate) fn p67_layout_slots(ctx: &dyn NativeContext, layout: ObjectRef) -> P67LayoutSlots {
    p67_layout_slots_inner(ctx, layout, None)
}

fn p67_layout_slots_inner(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    fabricated_name: Option<usize>,
) -> P67LayoutSlots {
    let class_id = ctx.class_id_of_object(layout);
    if let (Some(byte_size), Some(byte_alignment), Some(name)) = (
        ctx.resolve_field_index_by_class_id(class_id, "byteSize"),
        ctx.resolve_field_index_by_class_id(class_id, "byteAlignment"),
        ctx.resolve_field_index_by_class_id(class_id, "name"),
    ) {
        return P67LayoutSlots {
            byte_size,
            byte_alignment,
            name,
            order: match ctx.resolve_field_index_by_class_id(class_id, "order") {
                // A group layout (struct, union, padding, sequence) declares no
                // `order` at all and has no byte order to record; the flag slot
                // is never read for one, so the fabricated index is harmless.
                Some(i) => P67LayoutOrder::Object(i),
                None => P67LayoutOrder::Flag(2),
            },
            target: ctx
                .resolve_field_index_by_class_id(class_id, "targetLayout")
                .unwrap_or(4),
            payload: ctx
                .resolve_field_index_by_class_id(class_id, "elements")
                .or_else(|| ctx.resolve_field_index_by_class_id(class_id, "elementLayout"))
                .unwrap_or(2),
            element_count: ctx
                .resolve_field_index_by_class_id(class_id, "elemCount")
                .unwrap_or(4),
        };
    }
    // The fabricated map. `name` sits at 2 on the group carriers (struct, union,
    // padding: slot 2 is their member list and slot 3 the name) and at 3 on the
    // value carriers -- the historical test for which is "does slot 0 hold an
    // Int", kept verbatim so this arm answers exactly what the open-coded
    // readers answered.
    let name = match fabricated_name {
        Some(slot) => slot,
        None => {
            if matches!(ctx.get_field(layout, 0), Value::Int(_)) {
                2
            } else {
                3
            }
        }
    };
    P67LayoutSlots {
        byte_size: 0,
        byte_alignment: 1,
        name,
        order: P67LayoutOrder::Flag(2),
        target: 4,
        payload: 2,
        element_count: 4,
    }
}

/// The real JDK class behind a `java.lang.foreign` VALUE layout interface, or
/// `None` for anything this does not name -- the group layouts, and the impl
/// names themselves, which arrive here when a `withX` copy re-mints a carrier
/// from its own class name.
pub(crate) fn p67_value_layout_impl(class_name: &str) -> Option<&'static str> {
    Some(match class_name {
        "java/lang/foreign/ValueLayout$OfBoolean" => {
            "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl"
        }
        "java/lang/foreign/ValueLayout$OfByte" => {
            "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl"
        }
        "java/lang/foreign/ValueLayout$OfChar" => {
            "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl"
        }
        "java/lang/foreign/ValueLayout$OfShort" => {
            "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl"
        }
        "java/lang/foreign/ValueLayout$OfInt" => {
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl"
        }
        "java/lang/foreign/ValueLayout$OfLong" => {
            "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl"
        }
        "java/lang/foreign/ValueLayout$OfFloat" => {
            "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl"
        }
        "java/lang/foreign/ValueLayout$OfDouble" => {
            "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl"
        }
        "java/lang/foreign/AddressLayout" => {
            "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl"
        }
        _ => return None,
    })
}

/// Record `little` on `layout`, in whichever way its carrier records it.
///
/// The real arm ALLOCATES a `ByteOrder`, which can move `layout` -- the native
/// stale-local family -- so the carrier is pinned across it.
pub(crate) fn p67_layout_set_order(
    ctx: &mut dyn NativeContext,
    layout: ObjectRef,
    little: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    match p67_layout_slots(ctx, layout).order {
        P67LayoutOrder::Flag(slot) => {
            if ctx.object_num_fields(layout) > slot {
                ctx.set_field(layout, slot, Value::Int(i32::from(little)));
            }
            Ok(layout)
        }
        P67LayoutOrder::Object(slot) => {
            let pin = ctx.pin_native_root(layout);
            let order = p67_byte_order_object(ctx, little)?;
            let layout = ctx.read_native_pin(pin, layout);
            ctx.unpin_native_roots(pin);
            ctx.set_field(layout, slot, Value::Object(Some(order)));
            Ok(layout)
        }
    }
}

pub(crate) fn p67_layout_name_value(ctx: &dyn NativeContext, layout: ObjectRef) -> Value {
    let slots = p67_layout_slots(ctx, layout);
    if ctx.object_num_fields(layout) > slots.name {
        ctx.get_field(layout, slots.name)
    } else {
        Value::Object(None)
    }
}

pub(crate) fn p67_layout_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = p67_layout_name_value(ctx, this);
    Ok(Some(Value::Object(Some(p67_optional(ctx, name)?))))
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

pub(crate) fn p67_class_mirror(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match class_name {
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void" => {
            Ok(ctx.primitive_class_mirror(class_name))
        }
        _ => {
            if let Some(cid) = ctx.class_id_by_name(class_name) {
                return Ok(ctx.get_class_mirror(cid));
            }
            if let Ok(cid) = ctx.ensure_class_initialized(class_name) {
                return Ok(ctx.get_class_mirror(cid));
            }
            try_alloc_concurrent_synthetic(ctx, "java/lang/Class", 2)
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
    )?))))
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
    // Through the resolver, not a second copy of the rule: the open-coded
    // version here answered 3 for a real value layout, which is `carrier`.
    let name_slot = p67_layout_slots(ctx, this).name;
    let clone_fields = std::cmp::max(field_count, name_slot + 1);
    let this_pin = ctx.pin_native_root(this);
    let name_pin = match name {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let cloned = try_alloc_concurrent_synthetic(ctx, &class_name, clone_fields)?;
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

/// `MemoryLayout.withByteAlignment(long)` — a COPY of the receiver carrying the
/// requested alignment.
///
/// # What this replaces
///
/// All nine registrations of this method (three interface names × the
/// `MemoryLayout`/`ValueLayout`/specific descriptor triple, plus the seven
/// `ValueLayout$Of*` classes) were `p67_return_this` — the receiver, unchanged.
/// A layout's alignment is the ONLY thing the method exists to change, so the
/// answer was wrong for every caller that asked, and wrong quietly: the
/// returned object is a perfectly good layout of the original alignment.
///
/// It is not a corner. `jdk.incubator.vector` builds its element layout as
///
/// ```text
/// IntVector.<clinit>:  ELEMENT_LAYOUT = ValueLayout.JAVA_INT.withByteAlignment(1)
/// ```
///
/// and every `intoMemorySegment` / `fromMemorySegment` store and load goes
/// through it. With the no-op, `IntVector.memorySegmentSet` asked a `byte[]`-
/// backed segment for a 4-byte-aligned write and `heap_segment_check_access`
/// correctly refused:
///
/// ```text
/// IllegalArgumentException: Target offset 0 is incompatible with alignment
///   constraint 4 for segment MemorySegment{ kind: heap, address: 0x0, byteSize: 16 }
/// ```
///
/// The refusal was right and the layout it was handed was wrong — which is the
/// worst shape a stub can take, because the error names the innocent half.
///
/// # The copy
///
/// Same shape as [`p67_layout_with_name`] next door, and for the same reason: a
/// layout is a value, `withByteAlignment` is `@Override`-free on every JDK
/// implementation and returns a NEW layout, and mutating the receiver would
/// change `ValueLayout.JAVA_INT` itself — a static every FFM caller in the VM
/// shares.
///
/// Slot 1 is the alignment on BOTH carriers, which is what lets one body serve
/// a real-JDK receiver and a CratonVM-minted one: `AbstractLayout` declares
/// `byteSize` then `byteAlignment`, and CratonVM's fabricated value-layout model
/// is `(byteSize, byteAlignment, littleEndianFlag, name)`. `p67_layout_size_align`
/// and `p67_layout_byte_alignment` already read it at that index from both.
///
/// # The refusal
///
/// `MemoryLayout.withByteAlignment` throws `IllegalArgumentException` for a
/// `byteAlignment` that is not a positive power of two (JDK 25 javadoc, and
/// `AbstractLayout`'s constructor enforces it). Answering a layout with a
/// nonsense alignment instead would push the failure to whichever accessor next
/// consulted it, which is exactly how the no-op above stayed invisible.
pub(crate) fn p67_layout_with_byte_alignment(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let alignment = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    };
    if alignment <= 0 || (alignment & (alignment - 1)) != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Invalid alignment constraint: {alignment}"),
        }
        .into());
    }
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "java/lang/foreign/MemoryLayout".to_string());
    // At least two slots, so a two-slot carrier that has never carried an
    // explicit alignment gains the slot rather than dropping the write. The
    // `max` with the receiver's own count is what keeps a real-JDK layout's
    // `name`/`carrier`/`order` fields (slots 2..4) alive across the copy.
    let field_count = ctx.object_num_fields(this);
    let clone_fields = std::cmp::max(field_count, 2);
    let this_pin = ctx.pin_native_root(this);
    let cloned = try_alloc_concurrent_synthetic(ctx, &class_name, clone_fields)?;
    let this = ctx.read_native_pin(this_pin, this);
    for i in 0..field_count {
        ctx.set_field(cloned, i, ctx.get_field(this, i));
    }
    ctx.unpin_native_roots(this_pin);
    let alignment_slot = p67_layout_slots(ctx, cloned).byte_alignment;
    ctx.set_field(cloned, alignment_slot, Value::Long(alignment));
    Ok(Some(Value::Object(Some(cloned))))
}

pub(crate) fn p67_address_layout_target_layout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let target_slot = p67_layout_slots(ctx, this).target;
    let target = if ctx.object_num_fields(this) > target_slot {
        ctx.get_field(this, target_slot)
    } else {
        Value::Object(None)
    };
    Ok(Some(Value::Object(Some(p67_optional(ctx, target)?))))
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
    // At least 5 for the fabricated carrier; a real `OfAddressImpl` declares
    // seven fields and `field_count` already reflects that.
    let clone_fields = std::cmp::max(field_count, 5);
    let this_pin = ctx.pin_native_root(this);
    let target_pin = match target {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let cloned = try_alloc_concurrent_synthetic(ctx, &class_name, clone_fields)?;
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
    let target_slot = p67_layout_slots(ctx, cloned).target;
    ctx.set_field(cloned, target_slot, target);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(cloned))))
}

pub(crate) fn p67_set_value_layout_static(
    ctx: &mut dyn NativeContext,
    field_name: &str,
    class_name: &str,
    byte_size: i64,
    byte_alignment: i64,
) -> Result<(), MethodCallFailed> {
    let obj = p67_layout_object(ctx, class_name, byte_size, byte_alignment)?;
    ctx.set_static_field_by_name(
        "java/lang/foreign/ValueLayout",
        field_name,
        Value::Object(Some(obj)),
    );
    Ok(())
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
        p67_set_value_layout_static(ctx, field_name, class_name, byte_size, byte_alignment)?;
    }
    Ok(None)
}

/// `(byteSize, byteAlignment)` of a layout carrier.
///
/// Every layout this file mints keeps the same two-slot prefix —
/// `[0]=byteSize, [1]=byteAlignment` — see [`p67_layout_object`]. The
/// alignment fallback is `size` because that is what a `ValueLayout`'s natural
/// alignment is; a zero would make the rounding below divide by zero.
pub(crate) fn p67_layout_size_align(ctx: &mut dyn NativeContext, layout: ObjectRef) -> (i64, i64) {
    let slots = p67_layout_slots(ctx, layout);
    let size = match ctx.get_field(layout, slots.byte_size) {
        Value::Long(v) => v,
        Value::Int(v) => i64::from(v),
        _ => 0,
    };
    let align = match ctx.get_field(layout, slots.byte_alignment) {
        Value::Long(v) if v > 0 => v,
        Value::Int(v) if v > 0 => i64::from(v),
        _ => size.max(1),
    };
    (size.max(0), align.max(1))
}

pub(crate) fn p67_layout_byte_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let slots = p67_layout_slots(ctx, this);
    Ok(Some(ctx.get_field(this, slots.byte_size)))
}

pub(crate) fn p67_layout_byte_alignment(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let slots = p67_layout_slots(ctx, this);
    let value = ctx.get_field(this, slots.byte_alignment);
    match value {
        Value::Long(_) => Ok(Some(value)),
        _ => Ok(Some(ctx.get_field(this, slots.byte_size))),
    }
}

pub(crate) fn p67_return_this(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

/// JDK-ONLY-LAYOUT (kind 2): converted from a raw slot-2 read behind a FIELD
/// COUNT to a by-NAME read on the receiver.
///
/// Slot 2 is CratonVM's fabricated `(byteSize, byteAlignment, littleEndianFlag,
/// name)` value-layout model. On a real `jdk.internal.foreign.layout.
/// ValueLayouts$AbstractValueLayout` the first two coincide -- `AbstractLayout`
/// declares `byteSize` and `byteAlignment` in that order -- and slot 2 does
/// NOT: it is `name:Optional<String>`, with `carrier` and the real `order` at 3
/// and 4.
///
/// The `object_num_fields(layout) <= 2` test in front of it was the SIXTH
/// count-based layout guard this family has produced, and like the other five
/// it stopped an out-of-range read rather than a wrong-field one. It answered
/// correctly only because the FFM preseed hands out exactly two slots; the
/// moment a real `ValueLayout.<clinit>` builds the constants instead (which is
/// what `--jdk-only` does since 2026-08-10) the object has six fields, the read
/// lands on an unwritten `Optional` reference, and the R-niche rule decodes
/// that as `Int(0)` -- so every layout reported **BIG_ENDIAN** on a
/// little-endian host while `ByteOrder.nativeOrder()` next to it said
/// LITTLE_ENDIAN. Measured against Temurin 25.0.3 by
/// `probes/W2ValueLayoutProbe`.
pub(crate) fn p67_layout_is_little(ctx: &dyn NativeContext, layout: ObjectRef) -> bool {
    // Real layout: it declares `order`, a `java.nio.ByteOrder` reference, and
    // that object declares `name`.
    if let Value::Object(Some(order_obj)) = ctx.get_field_by_name(layout, "order") {
        if let Value::Object(Some(name_obj)) = ctx.get_field_by_name(order_obj, "name") {
            if let Some(n) = ctx.read_string(name_obj) {
                return n != "BIG_ENDIAN";
            }
        }
        // A FABRICATED `java.nio.ByteOrder` carries the flag as an Int at slot
        // 0 instead of a name -- see `p67_byte_order_object`, which writes both
        // when the class declares `name` and only the flag when it does not.
        if let Some(v) = ctx.get_field(order_obj, 0).as_int() {
            return v != 0;
        }
    }
    // Fabricated layout: slot 2 is the flag, and only OUR model has one. Asked
    // by NAME, not by count: a real value layout declares `carrier`.
    let class_id = ctx.class_id_of_object(layout);
    let is_real = ctx
        .declared_fields(class_id)
        .iter()
        .any(|f| !f.is_static && (f.name == "carrier" || f.name == "order"));
    if is_real {
        // A real layout with no readable `order` says nothing about byte order;
        // every JDK constant is built from `ByteOrder.nativeOrder()`.
        return cfg!(target_endian = "little");
    }
    // BOUNDS check, not a layout guard — the layout question was already
    // settled by name above. The FFM preseed hands out objects sized
    // `num_total_fields.max(2)`, which have no slot 2 to read; answering the
    // host's native order is what `p67_layout_object` would have written had
    // there been room. Keeping the two kinds of test apart is the point: the
    // old `<= 2` here was doing BOTH jobs, and the layout half of it was wrong.
    if ctx.object_num_fields(layout) <= 2 {
        return cfg!(target_endian = "little");
    }
    ctx.get_field(layout, 2)
        .as_int()
        .map(|v| v != 0)
        .unwrap_or(cfg!(target_endian = "little"))
}

/// A `java.nio.ByteOrder` for `MemoryLayout.order()`.
///
/// W6-3 slot-index audit. JDK 25 `java.nio.ByteOrder` declares exactly ONE
/// instance field — `private final java.lang.String name` (`javap -p`; the
/// `BIG_ENDIAN`/`LITTLE_ENDIAN`/`NATIVE_ORDER` constants are all static). So on
/// a real-JDK layout slot 0 is a String REFERENCE, and stamping the
/// little-endian flag there put an `Int` in a reference slot: the GC would scan
/// it as an oop, and real `ByteOrder.toString()` bytecode reads that same slot
/// as the name.
///
/// The flag write stays — it is the synthetic-stub layout, and every existing
/// reader (`p67_layout_is_little`, `reflect_invoke::vh_byte_order_is_little`)
/// falls back to it. On top of it, when the CLASS actually declares `name` at a
/// slot this object has, write a genuine String there. Three shapes, all
/// covered: a fabricated stub names its fields `_f0..`, so `name` does not
/// resolve and only the flag lands (byte-identical to before); a real
/// `java.nio.ByteOrder` resolves `name` to slot 0 and gets the String;
/// a shape where the resolved index is out of range is skipped rather than
/// written out of bounds.
/// `java.nio.ByteOrder.LITTLE_ENDIAN` / `BIG_ENDIAN`.
///
/// **The canonical static first, and a mint only as a fallback.** `ByteOrder`
/// has exactly two instances and every caller compares them with `==`:
/// `ByteOrder.nativeOrder()` is one of the two `public static final` fields,
/// and `ValueLayout.JAVA_INT.order()` has to BE that same object, not an equal
/// one. Minting a fresh carrier satisfies every null check, prints the right
/// name, and fails the only test anyone writes. MEASURED against HotSpot
/// 25.0.4+7, both modes (`apps/probes/FfmSegmentSweep.java`):
///
/// ```text
///   ValueLayout.JAVA_INT.order() == ByteOrder.nativeOrder()
///     HotSpot  true      CratonVM  false
/// ```
///
/// This is the same rule `canonical_enum_constant` states for `Thread$State`
/// and `W7-93` states for the `StackWalker` options — a native that answers a
/// class's own published constant must hand back the object that class's
/// `<clinit>` stored. `ByteOrder` is not an `enum`, but its two constants are
/// `static final` fields of its own type, which is the only property that rule
/// needs.
///
/// The mint is KEPT as the fallback because a fabricated `java.nio.ByteOrder`
/// stand-in has no statics to read, and `p67_layout_is_little` already knows
/// how to read the flag out of slot 0 of one. So a stripped image keeps the
/// behaviour it had; a real one gets identity.
fn p67_canonical_byte_order(
    ctx: &mut dyn NativeContext,
    little_endian: bool,
) -> Option<ObjectRef> {
    let cid = ctx.ensure_class_initialized("java/nio/ByteOrder").ok()?;
    // `ensure_class_initialized` fabricates a stand-in rather than failing, so
    // an `Ok` is not evidence the image has the class; the static lookup is.
    let name = if little_endian {
        "LITTLE_ENDIAN"
    } else {
        "BIG_ENDIAN"
    };
    let idx = ctx.static_field_index_by_name(cid, name)?;
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

pub(crate) fn p67_byte_order_object(
    ctx: &mut dyn NativeContext,
    little_endian: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(canonical) = p67_canonical_byte_order(ctx, little_endian) {
        return Ok(canonical);
    }
    let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteOrder", 1)?;
    ctx.set_field(obj, 0, Value::Int(if little_endian { 1 } else { 0 }));
    let cid = ctx.class_id_of_object(obj);
    // Bound before the `if let` so the immutable reborrow of `ctx` ends here
    // rather than spanning the block that needs `&mut ctx`.
    let name_slot = ctx.resolve_field_index_by_class_id(cid, "name");
    if let Some(slot) = name_slot {
        if slot < ctx.object_num_fields(obj) {
            // `create_string` allocates and can move `obj` (native stale-local
            // family) — pin it across the call.
            let obj_pin = ctx.pin_native_root(obj);
            let name = ctx.create_string(if little_endian {
                "LITTLE_ENDIAN"
            } else {
                "BIG_ENDIAN"
            });
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, slot, Value::Object(Some(name)));
            return Ok(obj);
        }
    }
    Ok(obj)
}

pub(crate) fn p67_layout_order(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let little_endian = p67_layout_is_little(ctx, this);
    Ok(Some(Value::Object(Some(p67_byte_order_object(
        ctx,
        little_endian,
    )?))))
}

pub(crate) fn p67_layout_with_order(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "java/lang/foreign/ValueLayout".to_string());
    let slots = p67_layout_slots(ctx, this);
    let byte_size = match ctx.get_field(this, slots.byte_size) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 1,
    };
    let byte_alignment = match ctx.get_field(this, slots.byte_alignment) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => byte_size,
    };
    let little = vh_byte_order_is_little(ctx, args.get(1));
    let inherited_name = p67_layout_name_value(ctx, this);
    let obj = p67_layout_object(ctx, &class_name, byte_size, byte_alignment)?;
    // `p67_layout_object` has already stamped the HOST's order; this is the
    // caller's, which is the whole point of `withOrder`.
    let obj = p67_layout_set_order(ctx, obj, little)?;
    let name_slot = p67_layout_slots(ctx, obj).name;
    ctx.set_field(obj, name_slot, inherited_name);
    Ok(Some(Value::Object(Some(obj))))
}

// -- jdk.internal.foreign.MemorySessionImpl — the session model --------------
//
// Every session native is force-dispatched over real bytecode
// (`interpreter::force_native_over_real_jdk_bytecode`), so this model is the
// ONLY lifetime bookkeeping FFM has in either mode. Four words, as written by
// [`p67_memory_session`]:
//
//   state    = `Int(1)` open, `Int(0)` closed
//   acquires = clients currently inside `whileAlive`/`acquire0` (`Int`)
//   owner    = owning `Thread` of a confined session, null otherwise
//   actions  = close actions: `Object[]` of `Runnable`/`ResourceCleanup`, null
//
// Their SLOTS are NOT fixed — see [`P67SessionSlots`]. They were, at 0..3, and
// that is the W7-89 defect: in Compatible mode the carrier is the real loaded
// `MemorySessionImpl`, so slots 0..3 already belong to that class's own four
// fields and two of them are declared REFERENCES. Everything below reads the
// map rather than an index.
// The synthetic `java.lang.foreign.Arena` (an instance of the INTERFACE class —
// in real-JDK mode `Arena.ofConfined()` never reaches `jdk.internal.foreign
// .ArenaImpl`, because this file's factories are what run). Slot 1 carries the
// arena's own session, so `scope()` has a stable identity to hand out and
// `close()` has something to close.
const P67_ARENA_OPEN: usize = 0;
const P67_ARENA_SESSION: usize = 1;
/// `1` when `close()` is allowed on this arena, `0` when it is not.
///
/// `Arena.global()` and `Arena.ofAuto()` are NON-CLOSEABLE in the JDK and
/// `ofConfined()`/`ofShared()` are closeable, but all four minted the same
/// two-slot carrier here — `p67_new_arena`'s only parameter was `confined`,
/// which records the owner thread and says nothing about closeability. So
/// nothing could refuse, and `Arena.global().close()` CLOSED THE GLOBAL ARENA.
/// MEASURED against HotSpot 25.0.4+7, both modes
/// (`apps/probes/FfmSegmentSweep.java`, message from `FfmMsgProbe`):
///
/// ```text
///   Arena.global().close()
///     HotSpot   UnsupportedOperationException: Attempted to close a non-closeable session
///     CratonVM  no-throw
/// ```
///
/// Not a missing exception so much as a missing lifetime: every segment
/// allocated from the global arena is supposed to outlive everything, and a
/// `close()` that succeeds is a use-after-free waiting for its first reader.
///
/// The slot is APPENDED, so a carrier minted before this change (or by another
/// registrar) is simply narrower and reads as closeable — the pre-fix
/// behaviour — rather than reading garbage. `p67_arena_session` already guards
/// on `object_num_fields`, which is the pattern this follows.
const P67_ARENA_CLOSEABLE: usize = 2;
const P67_ARENA_SLOTS: usize = 3;

/// Slot 2 of a synthetic `MemorySegment` is its owning `Arena` — the convention
/// `panama.rs` already established for its 6-field segments ("no arena" at
/// field 2). Reading it is how `MemorySegment.scope()` returns the SAME session
/// object as the arena that allocated the segment.
const P67_SEGMENT_ARENA: usize = 2;

const P67_SESSION_STATE: usize = 0;
const P67_SESSION_ACQUIRES: usize = 1;
const P67_SESSION_OWNER: usize = 2;
const P67_SESSION_ACTIONS: usize = 3;
const P67_SESSION_SLOTS: usize = 4;

/// Where one particular session object keeps the model's four words.
///
/// **This indirection is the W7-89 repair.** The four constants above were used
/// as absolute slot indices, and in Compatible mode that is wrong: the carrier
/// class `jdk/internal/foreign/MemorySessionImpl` is the REAL, loaded JDK class,
/// which declares exactly four instance fields — `resourceList` and `owner` are
/// references, `state` and `acquireCount` are ints. Writing the model's `Int`
/// state word into slot 0 therefore wrote a primitive into a slot the class
/// types as a REFERENCE, and such a write does not read back as a `Value::Int`
/// (that family is W7-84-primitive-in-reference-store.md). So
/// [`p67_session_modelled`] — whose discriminator is exactly "slot 0 reads back
/// as an `Int`" — answered **false for every session this VM mints**, and with
/// it the whole FFM lifetime model went inert: `close()` recorded nothing,
/// `isAlive()` answered true forever, thread confinement never fired, and every
/// validity gate returned `Ok(())`. `panama.rs`'s twin (`pe_session_modelled`)
/// reads the same slot and was dead for the same reason.
///
/// That was **measured, not deduced**: `probes/MemorySessionIdentityProbe.java`
/// reports `arena.scope() == arena.scope()` as FALSE on CratonVM and true on
/// HotSpot 25.0.3.9. `Arena.scope()` is [`p67_receiver_session`], which can only
/// mint a fresh session when [`p67_arena_session`] rejects the one it is handed,
/// and every other predicate in that chain is separately measured true (the
/// arena is 2 slots wide, its class name is `java/lang/foreign/Arena`, slot 1
/// holds the session, and the session's class name is `MemorySessionImpl` with
/// four declared fields). `p67_session_modelled` is the only remaining candidate.
///
/// Resolving BY NAME rather than permuting the constants is deliberate: it does
/// not depend on the real class's field ORDER, and it makes every write
/// type-correct — the model's two ints land in the two int fields and its two
/// references in the two reference fields, whichever slots those turn out to be.
/// All four names must resolve or none is used, so the two maps can never be
/// mixed.
#[derive(Clone, Copy)]
pub(crate) struct P67SessionSlots {
    pub(crate) state: usize,
    pub(crate) acquires: usize,
    pub(crate) owner: usize,
    pub(crate) actions: usize,
}

impl P67SessionSlots {
    /// The map for a carrier that declares no fields of its own — the
    /// `--synthetic-jdk` stub, and the `ClassId(0)` fallback arm. There the
    /// slots are untyped, the model owns them outright, and any `Value` round
    /// trips.
    const SYNTHETIC: Self = Self {
        state: P67_SESSION_STATE,
        acquires: P67_SESSION_ACQUIRES,
        owner: P67_SESSION_OWNER,
        actions: P67_SESSION_ACTIONS,
    };

    /// How many slots an object must carry for all four indices to be in bounds.
    pub(crate) fn required_width(self) -> usize {
        1 + self
            .state
            .max(self.acquires)
            .max(self.owner)
            .max(self.actions)
    }
}

/// The slot map for `session`. See [`P67SessionSlots`].
///
/// The names are `MemorySessionImpl`'s own: `state` and `acquireCount` are the
/// two ints, `owner` is the confining `Thread`, and `resourceList` is the
/// close-action list — which is what the model's `actions` array IS, so the
/// mapping is semantic and not merely kind-compatible.
pub(crate) fn p67_session_slots(ctx: &dyn NativeContext, session: ObjectRef) -> P67SessionSlots {
    let class_id = ctx.class_id_of_object(session);
    match (
        ctx.resolve_field_index_by_class_id(class_id, "state"),
        ctx.resolve_field_index_by_class_id(class_id, "acquireCount"),
        ctx.resolve_field_index_by_class_id(class_id, "owner"),
        ctx.resolve_field_index_by_class_id(class_id, "resourceList"),
    ) {
        (Some(state), Some(acquires), Some(owner), Some(actions)) => P67SessionSlots {
            state,
            acquires,
            owner,
            actions,
        },
        _ => P67SessionSlots::SYNTHETIC,
    }
}

pub(crate) fn p67_memory_session(ctx: &mut dyn NativeContext) -> Result<Value, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(
        ctx,
        "jdk/internal/foreign/MemorySessionImpl",
        P67_SESSION_SLOTS,
    )?;
    let slots = p67_session_slots(ctx, obj);
    ctx.set_field(obj, slots.state, Value::Int(1));
    ctx.set_field(obj, slots.acquires, Value::Int(0));
    ctx.set_field(obj, slots.owner, Value::Object(None));
    ctx.set_field(obj, slots.actions, Value::Object(None));
    Ok(Value::Object(Some(obj)))
}

/// The session a segment or arena already owns, or a fresh one if it has none.
///
/// `scope()` has to return the SAME session object every time it is asked: a
/// freshly minted one is always open, so `segment.scope().isAlive()` would keep
/// answering true long after the owning arena closed, and the validity checks
/// below would never fire on the session that was actually closed. A real-JDK
/// receiver carries it in a named field (`AbstractMemorySegmentImpl.scope`,
/// `ArenaImpl.session`) — and that field holds one of OUR sessions, because the
/// `createConfined`/`createShared` factories are force-dispatched here. A
/// synthetic receiver has no such field; there, a fresh session is all there is.
pub(crate) fn p67_receiver_session(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
) -> Result<Value, MethodCallFailed> {
    // A real-JDK receiver carries it in a named field.
    for name in ["scope", "session"] {
        if let Value::Object(Some(session)) = ctx.get_field_by_name(receiver, name) {
            return Ok(Value::Object(Some(session)));
        }
    }
    // The receiver is itself a synthetic Arena.
    if let Some(session) = p67_arena_session(ctx, receiver) {
        return Ok(Value::Object(Some(session)));
    }
    // The receiver is a synthetic MemorySegment: slot 2 names the arena that
    // allocated it, and the answer is that arena's session — this is what makes
    // `arena.scope() == segment.scope()` hold.
    if ctx.object_num_fields(receiver) > P67_SEGMENT_ARENA {
        if let Value::Object(Some(owner)) = ctx.get_field(receiver, P67_SEGMENT_ARENA) {
            if let Some(session) = p67_arena_session(ctx, owner) {
                return Ok(Value::Object(Some(session)));
            }
            // G19-1: OR THE SLOT HOLDS THE SESSION ITSELF.
            //
            // `panama::pe_segment_slice` has stamped a slice's slot 2 with the
            // PARENT'S SESSION — not with an arena — since W7-89, and
            // `panama::pe_segment_session` has had a "tolerate a segment
            // stamped with the session directly" arm for exactly that shape
            // the whole time. This reader never grew the matching arm, so the
            // two files disagreed about what slot 2 can hold and every
            // `slice.scope()` fell through to the fresh mint below.
            //
            // MEASURED before this arm (`--jdk-only`, 25.0.3+9-LTS oracle):
            //
            //     conf.allocate(16).asSlice(4,4).scope() == seg.scope()
            //        CratonVM false   HotSpot true
            //     MemorySegment.ofArray(new byte[16]).scope() == ... .scope()
            //        CratonVM false   HotSpot true
            //
            // A fresh session is always open, so this was not only an identity
            // divergence: a slice of a CLOSED arena reported a live scope.
            //
            // `panama::pe_session_modelled` and NOT the local
            // `p67_session_modelled`: the local one is width-and-state-word
            // only, and slot 2's OTHER tenant on an `ofArray` mirror carrier is
            // the Java backing ARRAY. `object_num_fields`/`get_field` on an
            // array are not the two-int shape the local predicate assumes, so
            // recognising a session by shape alone here would risk reading an
            // `int[]`'s element 0 as a session state word — the class-name test
            // in panama's copy is exactly the guard that rules that out, and
            // it is memoised so the extra precision is an integer compare.
            if crate::panama::pe_session_modelled(ctx, owner) {
                return Ok(Value::Object(Some(owner)));
            }
        }
    }
    Ok(p67_memory_session(ctx)?)
}

/// The session stored on a synthetic Arena, if this object is one.
///
/// Self-validating rather than shape-guessing: the slot must actually hold a
/// session we modelled, so a segment (whose slot 1 is a `Long` address) and any
/// other 2+-slot object answer `None`.
fn p67_arena_session(ctx: &dyn NativeContext, arena: ObjectRef) -> Option<ObjectRef> {
    let slots = p67_arena_slots(ctx, arena);
    if ctx.object_num_fields(arena) <= slots.session {
        return None;
    }
    match ctx.get_field(arena, slots.session) {
        Value::Object(Some(session)) if p67_session_modelled(ctx, session) => Some(session),
        _ => None,
    }
}

/// The class an `Arena` carrier is minted as: the REAL JDK implementation.
///
/// It used to be the `java.lang.foreign.Arena` INTERFACE, which made
/// `Arena.ofConfined().getClass()` answer `java.lang.foreign.Arena` where
/// HotSpot answers `jdk.internal.foreign.ArenaImpl` -- an object whose class
/// `isInterface()` is true and whose `getSuperclass()` is null, which the Java
/// object model does not contain.
pub(crate) const P67_ARENA_IMPL: &str = "jdk/internal/foreign/ArenaImpl";

/// Enough slots for the real class's two fields plus the two flags this VM
/// keeps that the JDK has no field for. `try_alloc_concurrent_synthetic` takes
/// the LARGER of this and the loaded class's own field count, so the request is
/// a floor, not a claim about the layout.
const P67_ARENA_IMPL_SLOTS: usize = 4;

/// Where an `Arena`'s fields live, resolved from the receiver's class.
///
/// `ArenaImpl` is a real JDK class with two instance fields (`session`,
/// `shouldReserveMemory`); the interface carrier this file used to mint had
/// none, so its slot numbers were free to choose. Resolving by NAME is the
/// pattern [`p67_session_slots`] already uses for `MemorySessionImpl`, and it
/// is what lets these accessors keep serving a carrier minted before this
/// change -- by another registrar, or by a build that could not load the impl.
pub(crate) struct P67ArenaSlots {
    pub(crate) session: usize,
    pub(crate) open: usize,
    pub(crate) closeable: usize,
}

impl P67ArenaSlots {
    /// The historical interface carrier. Still reachable: `refused_class` hands
    /// back a synthetic stand-in when the impl class will not load.
    const SYNTHETIC: Self = Self {
        session: P67_ARENA_SESSION,
        open: P67_ARENA_OPEN,
        closeable: P67_ARENA_CLOSEABLE,
    };
}

pub(crate) fn p67_arena_slots(ctx: &dyn NativeContext, arena: ObjectRef) -> P67ArenaSlots {
    let class_id = ctx.class_id_of_object(arena);
    match ctx.resolve_field_index_by_class_id(class_id, "session") {
        // `open` and `closeable` have no JDK field to live in -- the JDK keeps
        // that state in the session, and expresses closeability by WHICH
        // session subclass it builds. They are APPENDED past the real layout
        // rather than aliased onto `shouldReserveMemory`, so nothing this VM
        // writes can be read back by real JDK bytecode as a field it declared.
        Some(session) => {
            let base = ctx.class_num_total_fields(class_id);
            P67ArenaSlots {
                session,
                open: base,
                closeable: base + 1,
            }
        }
        None => P67ArenaSlots::SYNTHETIC,
    }
}

/// Allocate a synthetic Arena together with the session that gives it a
/// lifetime. `confined` records the calling thread as the session owner, which
/// is what lets an off-thread access raise `WrongThreadException`; a shared or
/// automatic arena leaves the owner null.
fn p67_new_arena(
    ctx: &mut dyn NativeContext,
    confined: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    p67_new_arena_kind(ctx, confined, true)
}

/// [`p67_new_arena`] with the closeability the caller knows and the carrier
/// could not previously record. See [`P67_ARENA_CLOSEABLE`].
fn p67_new_arena_kind(
    ctx: &mut dyn NativeContext,
    confined: bool,
    closeable: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let arena = try_alloc_concurrent_synthetic(ctx, P67_ARENA_IMPL, P67_ARENA_IMPL_SLOTS)?;
    // The session allocation below can move the fresh arena (native stale-local
    // family).
    let arena_pin = ctx.pin_native_root(arena);
    let session_value = p67_memory_session(ctx)?;
    let arena = ctx.read_native_pin(arena_pin, arena);
    ctx.unpin_native_roots(arena_pin);
    // Resolved AFTER the allocation, from the object that actually came back:
    // if the impl class would not load, this is the synthetic carrier and its
    // slots are the historical ones.
    let slots = p67_arena_slots(ctx, arena);
    ctx.set_field(arena, slots.open, Value::Int(1));
    ctx.set_field(arena, slots.session, session_value);
    ctx.set_field(arena, slots.closeable, Value::Int(i32::from(closeable)));
    if confined {
        if let Value::Object(Some(session)) = session_value {
            let owner = ctx.current_thread_object();
            let slots = p67_session_slots(ctx, session);
            ctx.set_field(session, slots.owner, Value::Object(Some(owner)));
        }
    }
    Ok(arena)
}

/// Allocate a synthetic segment owned by `arena`.
///
/// Three slots, not the historical two: slot 2 records the owning arena so
/// `segment.scope()` can answer with the arena's session. The extra slot is
/// invisible to the existing readers, which discriminate the segment layouts by
/// field count at `>= 4` and `>= 6` (`p67_segment_parts`,
/// `p67_segment_byte_size`, `p67_segment_address`, and `panama_libffi
/// ::segment_address`) — a 3-field segment takes the same branches a 2-field
/// one did.
fn p67_arena_segment(
    ctx: &mut dyn NativeContext,
    arena: ObjectRef,
    size: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let arena_pin = ctx.pin_native_root(arena);
    let segment = crate::panama::alloc_segment_carrier(ctx, 3)?;
    let arena = ctx.read_native_pin(arena_pin, arena);
    ctx.unpin_native_roots(arena_pin);
    ctx.set_field(segment, 0, Value::Long(size));
    ctx.set_field(segment, 1, Value::Long(0)); // address
    p67_stamp_segment_arena(ctx, segment, arena);
    Ok(segment)
}

/// Stamp `arena` onto a freshly allocated synthetic segment so the segment can
/// report the arena's session as its scope. Silently does nothing for a segment
/// too small to carry the slot, which keeps the older 2-field callers valid.
fn p67_stamp_segment_arena(ctx: &dyn NativeContext, segment: ObjectRef, arena: ObjectRef) {
    if ctx.object_num_fields(segment) > P67_SEGMENT_ARENA {
        ctx.set_field(segment, P67_SEGMENT_ARENA, Value::Object(Some(arena)));
    }
}

/// Whether `session` is a REAL, JDK-bytecode-backed session rather than the
/// stand-in [`p67_memory_session`] builds.
///
/// Two independent signals, both required:
///
///  * the class declares the JDK's own named `state` field (`MemorySessionImpl`
///    has exactly `resourceList`, `owner`, `state`, `acquireCount`), and
///  * it is not the abstract base itself — every real session is a CONCRETE
///    subclass (`ConfinedSession`, `SharedSession`, `GlobalSession`,
///    `ImplicitSession`), whereas our stand-in is an instance of the abstract
///    class and has no `justClose`/`acquire0` body to run.
fn p67_session_is_real(ctx: &dyn NativeContext, session: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(session);
    ctx.resolve_field_index_by_class_id(class_id, "state")
        .is_some()
        && ctx.class_name_arc_of_id(class_id).as_deref()
            != Some("jdk/internal/foreign/MemorySessionImpl")
}

/// Hand a session call back to the receiver's own JDK bytecode when the
/// receiver is a real session; `None` means "not a real session, use the model
/// below".
///
/// These natives are force-dispatched over bytecode
/// (`force_native_over_real_jdk_bytecode`), so in real-JDK mode they are the
/// ONLY thing that runs — and the object they are handed is a real
/// `ConfinedSession`/`SharedSession` built by `Arena.ofConfined()`'s own
/// bytecode, whose `state`/`acquireCount`/`resourceList` are real fields with a
/// real lifecycle. Re-implementing that lifecycle on top of a foreign layout
/// would be guesswork; running the JDK's own body is exact. This is the same
/// real-receiver escape `ThreadPoolExecutor.execute` uses, and
/// `invoke_virtual_bytecode_only` is the primitive built for it: it goes
/// straight to `interpreter::execute` and does NOT re-enter this native.
///
/// The neighbouring segment natives already work this way — `p67_segment_parts`
/// reads a real segment's `min`/`length` BY NAME — which is why raw segment
/// access works in real-JDK mode while the session lifecycle did not.
fn p67_session_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method_name: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return None;
    };
    if !p67_session_is_real(ctx, this) {
        return None;
    }
    Some(ctx.invoke_virtual_bytecode_only(this, method_name, descriptor, &args[1..]))
}

/// Raise the JDK's own `IllegalStateException` if `segment`'s scope has been
/// closed.
///
/// Once the session natives actually close a session, the arena's
/// `resourceList.cleanup()` really does free the off-heap block — so the
/// unchecked raw load/store in [`p67_segment_get_width`]/
/// [`p67_segment_set_width`] would turn a use-after-close from a harmless wrong
/// answer into a use-after-FREE. Real segments carry their session in the named
/// `scope` field (`AbstractMemorySegmentImpl.scope`); synthetic ones have no
/// such field and are unaffected.
/// The receiver is `&mut` because this function can RELOCATE it:
/// `checkValidState()` below is Java bytecode, so the collector can run inside
/// it and forward `segment`. Taking it by value left that entirely to the
/// callers, and they split two ways — `p67_segment_get_string` and
/// `lang_invoke`'s VarHandle-segment shape had each pinned around this call by
/// hand, with a comment saying exactly why, while the three sites beside them
/// (`p67_segment_get_width`, and both calls in `p67_segment_set_width`) went
/// straight on to `p67_segment_parts`, which reads the receiver's fields. That
/// is the ratio `scripts/stale-receiver-audit.py`'s header predicts, and `&mut`
/// is its prescribed remedy: it makes an unconverted caller a COMPILE ERROR
/// instead of something the next audit has to find again.
pub(crate) fn p67_segment_check_scope(
    ctx: &mut dyn NativeContext,
    segment: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    if let Value::Object(Some(mut scope)) = ctx.get_field_by_name(*segment, "scope") {
        if p67_session_is_real(ctx, scope) {
            // The one GC point in this function. Pin across it and hand the
            // caller the forwarded reference; every other branch below only
            // reads fields and cannot move anything.
            let pin = ctx.pin_native_root(*segment);
            let checked =
                ctx.invoke_virtual_bytecode_only(scope, "checkValidState", "()V", &[]);
            *segment = ctx.read_native_pin(pin, *segment);
            ctx.unpin_native_roots(pin);
            checked?;
            return Ok(());
        }
        // W7-89: a REAL segment can carry one of OUR sessions. The
        // `createConfined`/`createShared`/`createHeap` factories are
        // force-dispatched into this file, so `MemorySegment.ofArray(...)`
        // builds a genuine `HeapMemorySegmentImpl` whose `scope` field holds a
        // synthetic `MemorySessionImpl` — measured, `MemorySessionShapeProbe`
        // reports `heap.field.scope = jdk.internal.foreign.MemorySessionImpl` on
        // CratonVM against `GlobalSession$HeapSession` on HotSpot. Returning
        // unconditionally here was a second fail-open: the one shape that
        // resolves a session was the one shape that skipped the check.
        if p67_session_modelled(ctx, scope) {
            p67_session_check_valid(ctx, &mut scope)?;
            return Ok(());
        }
        // Neither — so this is not a scope at all, and accepting it would be a
        // third fail-open. `get_field_by_name` resolves an index in the LOADED
        // class's hierarchy, and a synthetic segment is stamped with the
        // `MemorySegment` INTERFACE, so the name can land on a slot this model
        // owns rather than on a real `AbstractMemorySegmentImpl.scope`. Fall
        // through to the arena, which is self-validating.
    }
    // Synthetic segment: slot 2 names the owning arena. Deliberately NOT
    // `p67_receiver_session`, which mints a fresh (always-open) session when it
    // finds nothing — that would make every check trivially pass.
    if ctx.object_num_fields(*segment) > P67_SEGMENT_ARENA {
        if let Value::Object(Some(mut owner)) = ctx.get_field(*segment, P67_SEGMENT_ARENA) {
            if let Some(mut session) = p67_arena_session(ctx, owner) {
                p67_session_check_valid(ctx, &mut session)?;
            } else if crate::panama::pe_session_modelled(ctx, owner) {
                // G19-1: the slot's third tenant — the session ITSELF, which is
                // what `panama::pe_segment_slice` stamps onto a slice and what
                // `pe_of_array_alias` now stamps onto a heap carrier. Without
                // this arm the one shape whose scope IS resolvable was the one
                // shape that skipped the check, which is the same fail-open
                // W7-89 closed one branch up.
                //
                // The class-name-checked predicate, for the reason spelled out
                // in `p67_receiver_session`: the local `p67_session_modelled`
                // would accept slot 2's OTHER tenant, an `ofArray` mirror's
                // backing array, and read an element as a state word.
                p67_session_check_valid(ctx, &mut owner)?;
            }
        }
    }
    Ok(())
}

/// Whether `session` carries the layout [`p67_memory_session`] writes.
///
/// The session natives are force-dispatched, so a session that real
/// `ConfinedSession`/`SharedSession` bytecode constructed can reach them too.
/// Anything we did not build is left strictly alone: it is neither interpreted
/// (so we never throw on a shape we misread) nor overwritten (so we never
/// corrupt a real object's fields).
///
/// **The real-session exclusion is now explicit** (W7-89). It used to be a side
/// effect of the slot-0 read: on a real `ConfinedSession` slot 0 is a reference
/// field, so the `Value::Int` test failed. Now that the state word is resolved
/// by NAME, a real session resolves `state` too — and its encoding is the JDK's
/// (`OPEN = 0`, `CLOSED = -1`, `NONCLOSEABLE = 1`), the exact inverse of this
/// model's `1 = open`. Interpreting one with the other's encoding would report
/// every live real session as closed, which is the over-correction this repair
/// must not commit. [`p67_session_is_real`] is the same test the delegation path
/// already uses, so the two agree by construction.
fn p67_session_modelled(ctx: &dyn NativeContext, session: ObjectRef) -> bool {
    if p67_session_is_real(ctx, session) {
        return false;
    }
    let slots = p67_session_slots(ctx, session);
    ctx.object_num_fields(session) >= slots.required_width()
        && matches!(ctx.get_field(session, slots.state), Value::Int(_))
}

fn p67_session_state(ctx: &dyn NativeContext, session: ObjectRef) -> i32 {
    let slots = p67_session_slots(ctx, session);
    match ctx.get_field(session, slots.state) {
        Value::Int(state) => state,
        _ => 1,
    }
}

fn p67_session_acquires(ctx: &dyn NativeContext, session: ObjectRef) -> i32 {
    let slots = p67_session_slots(ctx, session);
    match ctx.get_field(session, slots.acquires) {
        Value::Int(count) => count,
        _ => 0,
    }
}

/// `java.lang.WrongThreadException` for a confined session touched off-owner.
/// Built as the real class so `catch (WrongThreadException)` matches; if that
/// class cannot be constructed we still fail (with `IllegalStateException`)
/// rather than letting the wrong-thread access through.
fn p67_wrong_thread(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    const MESSAGE: &str = "Attempted access outside owning thread";
    let detail = ctx.create_string(MESSAGE);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/lang/WrongThreadException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IllegalStateException {
        message: MESSAGE.to_string(),
    }
    .into()
}

/// `MemorySessionImpl.checkValidStateRaw()`. Thread confinement is checked
/// before liveness — the JDK's order, so a confined session touched from the
/// wrong thread reports the thread error even once it has been closed.
fn p67_session_check_valid(
    ctx: &mut dyn NativeContext,
    session: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    // `&mut` receiver: see `ks_require_loaded` in `tls.rs` for why this is the
    // shape rather than a proof about today's callers.
    if !p67_session_modelled(ctx, *session) {
        return Ok(());
    }
    let slots = p67_session_slots(ctx, *session);
    let owner = match ctx.get_field(*session, slots.owner) {
        Value::Object(Some(owner)) => Some(owner),
        _ => None,
    };
    if let Some(owner) = owner {
        if ctx.current_thread_object() != owner {
            return Err(p67_wrong_thread(ctx));
        }
    }
    if p67_session_state(ctx, *session) == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Already closed".to_string(),
        }
        .into());
    }
    Ok(())
}

fn p67_session_acquire(
    ctx: &mut dyn NativeContext,
    session: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    p67_session_check_valid(ctx, session)?;
    if p67_session_modelled(ctx, *session) {
        let count = p67_session_acquires(ctx, *session);
        let slots = p67_session_slots(ctx, *session);
        ctx.set_field(*session, slots.acquires, Value::Int(count.saturating_add(1)));
    }
    Ok(())
}

/// The release half of `acquire0`/`whileAlive`. Deliberately total: it runs on
/// the unwind path of a throwing callback, where raising a second exception
/// would mask the first.
fn p67_session_release(ctx: &mut dyn NativeContext, session: ObjectRef) {
    if !p67_session_modelled(ctx, session) {
        return;
    }
    let count = p67_session_acquires(ctx, session);
    let slots = p67_session_slots(ctx, session);
    ctx.set_field(session, slots.acquires, Value::Int((count - 1).max(0)));
}

/// Append one close action to the session's `Object[]`, growing it by one.
///
/// Close actions are rare and few (one per allocated segment at worst), so the
/// copy-on-append array costs less than standing up a synthetic `ArrayList`
/// and keeps the whole list walkable by the GC as an ordinary reference array.
fn p67_session_push_action(ctx: &mut dyn NativeContext, session: ObjectRef, action: ObjectRef) {
    // Resolved once: the map is a property of the CLASS, so it is unaffected by
    // the moving collection `new_array` below can trigger and by the `session`
    // rebind that follows it.
    let slots = p67_session_slots(ctx, session);
    let previous = match ctx.get_field(session, slots.actions) {
        Value::Object(Some(actions)) => Some(actions),
        _ => None,
    };
    let len = match previous {
        Some(actions) => ctx.array_length(actions),
        None => 0,
    };
    // `new_array` can trigger a moving young collection, which relocates all
    // three of these references (native stale-local family).
    let session_pin = ctx.pin_native_root(session);
    let action_pin = ctx.pin_native_root(action);
    let previous_pin = previous.map(|actions| ctx.pin_native_root(actions));
    let grown = ctx.new_array(ArrayElementType::Reference, len + 1);
    let session = ctx.read_native_pin(session_pin, session);
    let action = ctx.read_native_pin(action_pin, action);
    if let (Some(actions), Some(pin)) = (previous, previous_pin) {
        let actions = ctx.read_native_pin(pin, actions);
        for index in 0..len {
            ctx.set_array_element(grown, index, ctx.get_array_element(actions, index));
        }
    }
    ctx.set_array_element(grown, len, Value::Object(Some(action)));
    ctx.set_field(session, slots.actions, Value::Object(Some(grown)));
    ctx.unpin_native_roots(session_pin);
}

/// Run the registered close actions newest-first — the order the JDK's
/// `ResourceList` unwinds in. The list is detached before the first callback so
/// a cleanup that re-enters `close()` cannot run it a second time, and the
/// first failure is remembered and rethrown only after every remaining cleanup
/// has had its turn (a leaked cleanup is worse than a late exception).
fn p67_session_run_close_actions(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if !p67_session_modelled(ctx, session) {
        return Ok(());
    }
    let slots = p67_session_slots(ctx, session);
    let actions = match ctx.get_field(session, slots.actions) {
        Value::Object(Some(actions)) => actions,
        _ => return Ok(()),
    };
    ctx.set_field(session, slots.actions, Value::Object(None));
    // Each `run()` re-enters the interpreter and can move the array.
    let actions_pin = ctx.pin_native_root(actions);
    let mut failure: Option<MethodCallFailed> = None;
    let mut index = ctx.array_length(actions);
    while index > 0 {
        index -= 1;
        let actions = ctx.read_native_pin(actions_pin, actions);
        let Value::Object(Some(action)) = ctx.get_array_element(actions, index) else {
            continue;
        };
        // `ResourceCleanup implements Runnable` in the JDK, so `run()` covers
        // both the `addCloseAction` and the `addInternal` flavours.
        if let Err(err) = ctx.invoke_virtual(action, "run", "()V", &[]) {
            if failure.is_none() {
                failure = Some(err);
            }
        }
    }
    ctx.unpin_native_roots(actions_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// `MemorySessionImpl.justClose()`: validate and flip the state word, without
/// running the resource list — `close()` is `justClose()` plus the cleanup run.
fn p67_session_just_close(
    ctx: &mut dyn NativeContext,
    session: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    p67_session_check_valid(ctx, session)?;
    if !p67_session_modelled(ctx, *session) {
        return Ok(());
    }
    let acquired = p67_session_acquires(ctx, *session);
    if acquired > 0 {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Session is acquired by {acquired} clients"),
        }
        .into());
    }
    let slots = p67_session_slots(ctx, *session);
    ctx.set_field(*session, slots.state, Value::Int(0));
    Ok(())
}

/// Shared body of `addCloseAction` / `addOrCleanupIfFail` / `addInternal` for a
/// SYNTHETIC session: all three register one action to run at close, and all
/// three reject a session that is already closed (registering on a dead session
/// would leak it). Each registration handles the real-receiver case itself,
/// because delegation has to name the exact method and descriptor it was
/// entered through.
fn p67_session_add_action_synthetic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    p67_session_check_valid(ctx, &mut this)?;
    let Some(Value::Object(Some(action))) = args.get(1) else {
        return Ok(None);
    };
    if p67_session_modelled(ctx, this) {
        p67_session_push_action(ctx, this, *action);
    }
    Ok(None)
}

pub(crate) fn p67_layout_width_obj(ctx: &dyn NativeContext, layout: ObjectRef) -> i32 {
    let slots = p67_layout_slots(ctx, layout);
    match ctx.get_field(layout, slots.byte_size) {
        Value::Long(v) if (1..=8).contains(&v) => v as i32,
        Value::Int(_) => match ctx.get_field(layout, slots.byte_alignment) {
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
    let members = match ctx.get_field(layout, p67_layout_slots(ctx, layout).payload) {
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

// `p67_memory_layout_path_target` lived here until 2026-08-12: a path walk
// that followed GROUP-BY-NAME elements only, silently `break`ing on anything
// else (so `sequenceElement()` addressed the sequence itself) and discarding
// the offset it walked past (so it could not serve `byteOffset` at all). It is
// replaced by `p67_layout_path_walk` below, which is the single walk both
// `byteOffset` and `varHandle` use — two consumers of one path cannot disagree
// about where the path lands if there is only one walker.

// ---------------------------------------------------------------------------
// Layout paths — `byteOffset(PathElement...)` and the var-handle path walk
// ---------------------------------------------------------------------------
//
// `MemoryLayout.byteOffset` and `MemoryLayout.varHandle` are the two consumers
// of a layout PATH, and until 2026-08-12 only the second existed here (and it
// followed group elements by NAME only, discarding the offset it walked past).
// `byteOffset` was registered nowhere that reaches a shipping binary —
// `panama.rs` has one, but its registrar is `register_pe_panama`, reached only
// from `register_synthetic_overrides`, i.e. the synthetic-JDK arm — so a real
// `struct.byteOffset(groupElement("c"))` resolved to the ABSTRACT interface
// declaration and raised `AbstractMethodError: ... has no Code attribute`
// (regression-suite `RJdkForeign.layouts`). Both now share one walk.

/// A layout's byte size in either model: this file's `(byteSize, …)` carriers
/// hold it as a `Long` in slot 0; `panama.rs`'s `(kind, byteSize, …)` carriers
/// hold a kind tag there and the size in slot 1. A real
/// `jdk.internal.foreign.layout.AbstractLayout` declares `byteSize` first, so
/// the slot-0 `Long` read covers it too.
pub(crate) fn p67_layout_size_of(ctx: &dyn NativeContext, layout: ObjectRef) -> i64 {
    let slots = p67_layout_slots(ctx, layout);
    match ctx.get_field(layout, slots.byte_size) {
        Value::Long(v) => v,
        _ => match ctx.get_field(layout, slots.byte_alignment) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        },
    }
}

/// A layout's byte alignment, defaulting to its size (which is what every
/// value layout uses) when the carrier does not record one separately.
pub(crate) fn p67_layout_align_of(ctx: &dyn NativeContext, layout: ObjectRef) -> i64 {
    let slots = p67_layout_slots(ctx, layout);
    let by_slot = match ctx.get_field(layout, slots.byte_alignment) {
        Value::Long(v) if v > 0 => v,
        _ => 0,
    };
    if by_slot > 0 {
        return by_slot;
    }
    p67_layout_size_of(ctx, layout).max(1)
}

// ---------------------------------------------------------------------------
// THE ONE LAYOUT CARRIER ENCODING (F16, 2026-08-13)
// ---------------------------------------------------------------------------
//
// Every `java.lang.foreign` layout this VM mints has the SAME head:
//
//     [0] Long  byteSize
//     [1] Long  byteAlignment
//     [2]       payload — endian flag (value), member ARRAY (group),
//               ELEMENT layout (sequence), null (padding)
//     [3]       name (Optional's value, or null)
//
// Slots 0 and 1 are not an invention: a real
// `jdk.internal.foreign.layout.AbstractLayout` declares
//
//     $ sed -n '52,54p' jdk25src/java.base/jdk/internal/foreign/layout/AbstractLayout.java
//       private final long byteSize;
//       private final long byteAlignment;
//       private final Optional<String> name;
//
// in exactly that order, so the same two reads serve a CratonVM carrier and a
// real JDK layout object alike. That is why this encoding — not `panama.rs`'s
// `[0]=Int(kind)` one — is the authoritative one, and `panama.rs` no longer
// mints a layout at all outside its own unit tests. See the banner above
// `register_pe_value_layout` there.
//
// A member whose slot 0 is NOT a `Long` is a carrier this VM does not
// understand. It must NOT be defaulted to a plausible number: that is the
// defect this consolidation removes (`panama.rs` read slot 0 as `Int(kind)`
// and fell through to `_ => 0`, which is `LAYOUT_BYTE`, so a 4-byte layout
// silently became a 1-byte one). `p67_member_size_align` answers `None` and
// every caller turns that into a named refusal.

/// `(byteSize, byteAlignment)` of a member layout, or `None` if the object is
/// not a layout carrier this VM minted.
///
/// Deliberately has no default arm. See the banner above.
fn p67_member_size_align(ctx: &dyn NativeContext, member: ObjectRef) -> Option<(i64, i64)> {
    let slots = p67_layout_slots(ctx, member);
    let size = match ctx.get_field(member, slots.byte_size) {
        Value::Long(v) => v,
        _ => return None,
    };
    let align = match ctx.get_field(member, slots.byte_alignment) {
        Value::Long(v) if v > 0 => v,
        // A carrier with a size but no recorded alignment is a value layout
        // whose alignment equals its size — the JDK's own rule for every
        // `ValueLayout` constant except the `_UNALIGNED` ones, which DO record
        // a separate 1 (measured: `JAVA_INT_UNALIGNED` byteSize=4 align=1).
        _ => size.max(1),
    };
    Some((size, align))
}

/// Render a layout the way the JDK's `MemoryLayout::toString` does, because
/// the one exception message that quotes a layout quotes it in this form.
///
/// Measured on the oracle (Microsoft build 25.0.3+9-LTS):
///
/// ```text
/// JAVA_BYTE b1   JAVA_BOOLEAN z1   JAVA_CHAR c2    JAVA_SHORT  s2
/// JAVA_INT  i4   JAVA_LONG    j8   JAVA_FLOAT f4   JAVA_DOUBLE d8
/// ADDRESS   a8   paddingLayout(3)  x3
/// sequenceLayout(2, JAVA_INT)        [2:i4]
/// structLayout(JAVA_INT, JAVA_INT)   [i4i4]
/// unionLayout(JAVA_BYTE, JAVA_INT)   [b1|i4]
/// JAVA_INT.withName("x")             i4(x)
/// JAVA_INT_UNALIGNED                 1%i4
/// ```
///
/// Value layouts, padding and named layouts are reproduced EXACTLY; a group or
/// sequence renders its bracket form from the carrier's payload slot. The
/// message is a diagnostic, not a contract — what a caller can `catch` is the
/// KIND, `IllegalArgumentException`, which is exact.
pub(crate) fn p67_layout_render(ctx: &dyn NativeContext, layout: ObjectRef) -> String {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_default();
    let (size, align) = p67_member_size_align(ctx, layout).unwrap_or((0, 1));

    let base = if class_name.contains("PaddingLayout") {
        format!("x{size}")
    } else if class_name.contains("SequenceLayout") {
        let slots = p67_layout_slots(ctx, layout);
        let elem = match ctx.get_field(layout, slots.payload) {
            Value::Object(Some(e)) => p67_layout_render(ctx, e),
            _ => String::new(),
        };
        // The STORED count (slot 4) first — see the `elementCount()`
        // registration. Division is the fallback for a four-slot carrier and
        // is wrong whenever the element's byteSize is 0: the oracle renders
        // `sequenceLayout(3, structLayout())` as `[3:[]]`, and the division
        // would print `[0:[]]`.
        let stored = if ctx.object_num_fields(layout) > slots.element_count {
            // Read the count slot only AFTER the width check — a four-slot
            // carrier has no slot 4 to read, and asking for one is the
            // out-of-bounds field access this file's carrier notes keep
            // warning about.
            match ctx.get_field(layout, slots.element_count) {
                Value::Long(count) => Some(count),
                _ => None,
            }
        } else {
            None
        };
        let count = match stored {
            Some(count) => count,
            None => match ctx.get_field(layout, slots.payload) {
                Value::Object(Some(e)) => {
                    let es = p67_layout_size_of(ctx, e);
                    if es > 0 {
                        size / es
                    } else {
                        0
                    }
                }
                _ => 0,
            },
        };
        format!("[{count}:{elem}]")
    } else if class_name.contains("StructLayout")
        || class_name.contains("UnionLayout")
        || class_name.contains("GroupLayout")
    {
        let sep = if class_name.contains("UnionLayout") {
            "|"
        } else {
            ""
        };
        let parts = match ctx.get_field(layout, p67_layout_slots(ctx, layout).payload) {
            Value::Object(Some(arr)) => (0..ctx.array_length(arr))
                .map(|i| match ctx.get_array_element(arr, i) {
                    Value::Object(Some(m)) => p67_layout_render(ctx, m),
                    _ => String::new(),
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        format!("[{}]", parts.join(sep))
    } else {
        // A value layout. The JDK's code letter is its CARRIER, and
        // `p67_layout_carrier_name` already maps every `$Of*` spelling — real
        // and synthetic — onto that carrier, so the two cannot drift.
        let letter = match p67_layout_carrier_name(&class_name) {
            "boolean" => "z",
            "byte" => "b",
            "char" => "c",
            "short" => "s",
            "int" => "i",
            "long" => "j",
            "float" => "f",
            "double" => "d",
            "java/lang/foreign/MemorySegment" => "a",
            _ => "?",
        };
        format!("{letter}{size}")
    };

    // A VALUE layout whose alignment is not its size prints an `<align>%`
    // prefix (`JAVA_INT_UNALIGNED` -> `1%i4`). Padding, groups and sequences
    // never do: measured, `paddingLayout(3)` is `x3` (align 1, size 3) and
    // `structLayout(JAVA_INT, JAVA_INT)` is `[i4i4]` (align 4, size 8).
    let is_value = !(class_name.contains("PaddingLayout")
        || class_name.contains("SequenceLayout")
        || class_name.contains("StructLayout")
        || class_name.contains("UnionLayout")
        || class_name.contains("GroupLayout"));
    let with_align = if is_value && align != size.max(1) {
        format!("{align}%{base}")
    } else {
        base
    };
    match p67_layout_name_value(ctx, layout) {
        Value::Object(Some(s)) => match ctx.read_string(s) {
            Some(n) => format!("{with_align}({n})"),
            None => with_align,
        },
        _ => with_align,
    }
}

/// One element of a layout path, decoded from whichever carrier produced it.
///
/// `MemoryLayout.PathElement.groupElement(...)` / `sequenceElement(...)` run
/// REAL JDK bytecode here (measured: `groupElement("c")` yields a
/// `jdk.internal.foreign.LayoutPath$GroupElementByName`), so the decoding is
/// primarily by the real record classes' own field names; the synthetic
/// 2-field `MemoryLayout$PathElement` carrier is still accepted.
pub(crate) enum P67PathElement {
    /// `groupElement(String)` — a member of a struct/union, by name.
    GroupByName(String),
    /// `groupElement(long)` — a member by position.
    GroupByIndex(i64),
    /// `sequenceElement()` — an OPEN index: contributes no fixed offset and
    /// adds a `long` coordinate to a var handle.
    SequenceOpen,
    /// `sequenceElement(long)` — a fixed index.
    SequenceAt(i64),
    /// Anything this VM cannot decode. Carries the element's class name so the
    /// refusal can name it rather than answering a plausible zero.
    Unsupported(String),
}

pub(crate) fn p67_classify_path_element(
    ctx: &dyn NativeContext,
    elem: ObjectRef,
) -> P67PathElement {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(elem))
        .unwrap_or_default();
    if class_name.contains("SequenceElementByIndex") {
        return match ctx.get_field_by_name(elem, "index") {
            Value::Long(v) => P67PathElement::SequenceAt(v),
            Value::Int(v) => P67PathElement::SequenceAt(v as i64),
            _ => P67PathElement::SequenceOpen,
        };
    }
    if class_name.contains("SequenceElementByRange") {
        // A range element addresses a SLICE, not one element; refusing is the
        // honest answer rather than reporting the range's start.
        return P67PathElement::Unsupported(class_name);
    }
    if class_name.contains("SequenceElement") {
        return P67PathElement::SequenceOpen;
    }
    if class_name.contains("GroupElementByIndex") {
        return match ctx.get_field_by_name(elem, "index") {
            Value::Long(v) => P67PathElement::GroupByIndex(v),
            Value::Int(v) => P67PathElement::GroupByIndex(v as i64),
            _ => P67PathElement::Unsupported(class_name),
        };
    }
    if let Some(name) = p67_path_element_group_name(ctx, elem) {
        return P67PathElement::GroupByName(name);
    }
    P67PathElement::Unsupported(class_name)
}

/// The member layouts of a group layout (slot 2), if it has any.
fn p67_group_members(ctx: &dyn NativeContext, layout: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(layout, p67_layout_slots(ctx, layout).payload) {
        Value::Object(Some(arr)) if ctx.array_length(arr) > 0 => Some(arr),
        _ => None,
    }
}

/// Offset of a group member, computed with the SAME alignment rule
/// `structLayout` used to size the group — one algorithm, so a member's offset
/// and the group's size can never disagree.
///
/// `select` receives each member's index and name; the first member it accepts
/// stops the walk. Returns `(offset, member layout)`.
fn p67_group_member_offset(
    ctx: &dyn NativeContext,
    group: ObjectRef,
    mut select: impl FnMut(usize, Option<&str>) -> bool,
) -> Option<(i64, ObjectRef)> {
    let members = p67_group_members(ctx, group)?;

    // A UNION PUTS EVERY MEMBER AT OFFSET 0 (F16, 2026-08-13). This loop used
    // to accumulate for any group, which was invisible while `unionLayout`
    // discarded its members — `p67_group_members` answered `None` and this
    // function never ran on a union. Now that a union carries its members, the
    // path is reachable and has to be right. Measured on 25.0.3+9-LTS:
    //
    //     u = unionLayout(JAVA_BYTE.withName("b"), JAVA_INT.withName("i"),
    //                     JAVA_LONG.withName("l"))
    //     u.byteOffset(groupElement("b")) = 0
    //     u.byteOffset(groupElement("i")) = 0
    //     u.byteOffset(groupElement("l")) = 0
    let is_union = ctx
        .class_name_of_id(ctx.class_id_of_object(group))
        .is_some_and(|n| n.contains("UnionLayout"));

    let mut offset = 0_i64;
    for i in 0..ctx.array_length(members) {
        let member = match ctx.get_array_element(members, i) {
            Value::Object(Some(m)) => m,
            _ => continue,
        };
        // NO `align_up` HERE. A member's offset in a struct is the PLAIN
        // RUNNING SUM of the preceding members' sizes — the JDK does not
        // insert padding, it rejects a layout that needs some (see the
        // `structLayout` registration). Measured:
        //
        //     s  = struct(b1, x3, i4, j8)  -> b=0, i=4, l=8
        //     s2 = struct(j8, i4)          -> l=0, i=8   (size 12, NOT 16)
        //
        // The `offset = align_up(offset, align)` that used to be here was the
        // SECOND copy of the offset rule, and it disagreed with the first as
        // soon as the first stopped padding. It happened to be a no-op for
        // every layout `structLayout` will now build — the alignment check
        // guarantees the running offset is already a multiple of each member's
        // alignment — but a rule written twice is a rule that drifts, and this
        // copy silently rounded `struct(j8, i4)`'s second member to 8 for the
        // right reason and would have kept doing it for the wrong one.
        let name = p67_string_value(ctx, p67_layout_name_value(ctx, member));
        if select(i, name.as_deref()) {
            return Some((if is_union { 0 } else { offset }, member));
        }
        offset = offset.saturating_add(p67_layout_size_of(ctx, member).max(0));
    }
    None
}

/// The element layout of a sequence layout — slot 2 of the carrier
/// `MemoryLayout.sequenceLayout` mints. A GROUP layout's slot 2 is its member
/// ARRAY, so this is only ever asked of a receiver whose class says sequence.
fn p67_sequence_element_layout(ctx: &dyn NativeContext, layout: ObjectRef) -> Option<ObjectRef> {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_default();
    if !class_name.contains("SequenceLayout") {
        return None;
    }
    match ctx.get_field(layout, p67_layout_slots(ctx, layout).payload) {
        Value::Object(Some(e)) => Some(e),
        _ => None,
    }
}

/// [`p67_sequence_element_layout`] as a hard requirement: a sequence path
/// element applied to something that is not a sequence is the caller's error,
/// and the JDK reports it as `IllegalArgumentException`.
fn p67_sequence_element(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    p67_sequence_element_layout(ctx, layout).ok_or_else(|| {
        RuntimeError::IllegalArgumentException {
            message: "cannot resolve layout path element: the layout is not a sequence".to_string(),
        }
        .into()
    })
}

/// Where a layout path lands: the byte offset of the addressed element, the
/// element's own layout, and the STRIDE of each open (`sequenceElement()`)
/// index the path left behind.
///
/// The strides are what makes a var handle's extra `long` coordinates mean
/// something: `seq.varHandle(sequenceElement())` addresses
/// `base + index * elementSize`.
pub(crate) struct P67PathTarget {
    pub offset: i64,
    pub layout: ObjectRef,
    pub open_strides: Vec<i64>,
}

pub(crate) fn p67_layout_path_walk(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    path_arr: Option<ObjectRef>,
) -> Result<P67PathTarget, MethodCallFailed> {
    let mut current = layout;
    let mut offset = 0_i64;
    let mut open_strides: Vec<i64> = Vec::new();
    let Some(path_arr) = path_arr else {
        return Ok(P67PathTarget {
            offset,
            layout: current,
            open_strides,
        });
    };
    for i in 0..ctx.array_length(path_arr) {
        let elem = match ctx.get_array_element(path_arr, i) {
            Value::Object(Some(e)) => e,
            _ => continue,
        };
        match p67_classify_path_element(ctx, elem) {
            P67PathElement::GroupByName(name) => {
                let found = p67_group_member_offset(ctx, current, |_, member_name| {
                    member_name == Some(name.as_str())
                });
                match found {
                    Some((member_offset, member)) => {
                        offset = offset.saturating_add(member_offset);
                        current = member;
                    }
                    None => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: format!(
                                "cannot resolve layout path element: no member named `{name}`"
                            ),
                        }
                        .into());
                    }
                }
            }
            P67PathElement::GroupByIndex(idx) => {
                let found = p67_group_member_offset(ctx, current, |member_index, _| {
                    member_index as i64 == idx
                });
                match found {
                    Some((member_offset, member)) => {
                        offset = offset.saturating_add(member_offset);
                        current = member;
                    }
                    None => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: format!(
                                "cannot resolve layout path element: no member at index {idx}"
                            ),
                        }
                        .into());
                    }
                }
            }
            P67PathElement::SequenceOpen => {
                let element = p67_sequence_element(ctx, current)?;
                open_strides.push(p67_layout_size_of(ctx, element).max(0));
                current = element;
            }
            P67PathElement::SequenceAt(idx) => {
                let element = p67_sequence_element(ctx, current)?;
                let stride = p67_layout_size_of(ctx, element).max(0);
                offset = offset.saturating_add(stride.saturating_mul(idx));
                current = element;
            }
            P67PathElement::Unsupported(class_name) => {
                // Never a fabricated 0: a wrong offset is a silent wrong
                // read/write into somebody's off-heap memory.
                return Err(RuntimeError::UnsupportedOperationException {
                    message: format!(
                        "MemoryLayout path element `{class_name}` is not modelled by CratonVM; \
                         the offset is unknown rather than zero"
                    ),
                }
                .into());
            }
        }
    }
    Ok(P67PathTarget {
        offset,
        layout: current,
        open_strides,
    })
}

/// `MemoryLayout.byteOffset(PathElement...)`.
pub(crate) fn p67_layout_byte_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let path = match args.get(1) {
        Some(Value::Object(p)) => *p,
        _ => None,
    };
    let target = p67_layout_path_walk(ctx, this, path)?;
    if !target.open_strides.is_empty() {
        // `byteOffset` has no coordinates to supply an open index with; the JDK
        // rejects such a path outright.
        return Err(RuntimeError::IllegalArgumentException {
            message: "byteOffset does not accept an open sequence-element path".to_string(),
        }
        .into());
    }
    Ok(Some(Value::Long(target.offset)))
}

pub(crate) fn p67_var_handle_for_layout(
    ctx: &mut dyn NativeContext,
    layout: ObjectRef,
) -> Result<Value, MethodCallFailed> {
    p67_var_handle_for_path(ctx, layout, 0, &[])
}

/// Mint the FFM layout VarHandle for `layout`, addressing `base_offset` bytes
/// past its segment coordinate plus one `long` coordinate per entry in
/// `open_strides`.
///
/// The shape is recorded in `lang_invoke`'s side table, which is the ONLY place
/// this handle's meaning lives: the receiver is a synthetic
/// `java/lang/invoke/VarHandle` whose slots the real class declares as
/// `vform`/`… `, so slot reads are not a description. Before this recorded a
/// carrier, `varType()`/`coordinateTypes()` refused (kind 0 — they were reading
/// slot 0, this file's endianness flag, as a kind tag) and `get`/`set` fell
/// through to the instance-field path and silently no-opped: measured on the
/// shipping binary, `vhInt.set(seg, 0L, 11); vhInt.get(seg, 0L)` answered `0`
/// where HotSpot answers `11`.
pub(crate) fn p67_var_handle_for_path(
    ctx: &mut dyn NativeContext,
    layout: ObjectRef,
    base_offset: i64,
    open_strides: &[i64],
) -> Result<Value, MethodCallFailed> {
    if open_strides.len() > 1 {
        // A nested `sequenceElement(), sequenceElement()` path needs one index
        // coordinate per level, and the shape carries one. Refusing names the
        // gap; addressing with only the outer stride would read the wrong slot
        // and look like a working handle.
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!(
                "CratonVM models one open sequence index per layout var handle; this path has {}",
                open_strides.len()
            ),
        }
        .into());
    }
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_default();
    let carrier = p67_layout_carrier_name(&class_name);
    let little_endian = p67_layout_is_little(ctx, layout);
    let width = p67_layout_width_obj(ctx, layout);
    let shape = crate::lang_invoke::SegmentVhShape {
        width,
        carrier: p67_carrier_descriptor_byte(carrier, width),
        little_endian,
        base_offset,
        stride: open_strides.first().copied().unwrap_or(0),
    };
    let vh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS)?;
    ctx.set_field(
        vh,
        VH_CLASS_OR_TARGET,
        Value::Int(if little_endian { 1 } else { 0 }),
    );
    ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(width));
    ctx.set_field(vh, VH_IS_STATIC, Value::Int(VH_KIND_MEMORY_SEGMENT));
    crate::lang_invoke::register_p67_memory_segment_var_handle(ctx, vh, shape);
    Ok(Value::Object(Some(vh)))
}

/// The JVM descriptor byte for a layout carrier name, falling back to the
/// width when the carrier is not one of the eight primitives (`ADDRESS`, whose
/// carrier is `MemorySegment`, is addressed as a pointer-sized integer).
pub(crate) fn p67_carrier_descriptor_byte(carrier: &str, width: i32) -> u8 {
    match carrier {
        "boolean" => b'Z',
        "byte" => b'B',
        "char" => b'C',
        "short" => b'S',
        "int" => b'I',
        "long" => b'J',
        "float" => b'F',
        "double" => b'D',
        _ => {
            if width == 8 {
                b'J'
            } else {
                b'I'
            }
        }
    }
}

pub(crate) fn p67_var_handle(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Value, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(layout))) => p67_var_handle_for_layout(ctx, *layout),
        _ => {
            let vh =
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS)?;
            ctx.set_field(vh, VH_CLASS_OR_TARGET, Value::Int(1));
            ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(1));
            ctx.set_field(vh, VH_IS_STATIC, Value::Int(VH_KIND_MEMORY_SEGMENT));
            Ok(Value::Object(Some(vh)))
        }
    }
}

pub(crate) fn p67_memory_layout_var_handle(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let path = match args.get(1) {
        Some(Value::Object(p)) => *p,
        _ => None,
    };
    let target = p67_layout_path_walk(ctx, this, path)?;
    Ok(Some(p67_var_handle_for_path(
        ctx,
        target.layout,
        target.offset,
        &target.open_strides,
    )?))
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

/// `MemorySegment.isReadOnly()` — report the receiver's OWN read-only flag
/// instead of a blanket `false`.
///
/// Two receiver shapes reach this. A REAL `jdk.internal.foreign
/// .AbstractMemorySegmentImpl` keeps the flag in a concrete `readOnly` field
/// and implements `isReadOnly()` in bytecode that every subclass inherits — so
/// the constant `false` that used to be registered on the impl classes below
/// SHADOWED that concrete method (FACT 1) and told callers that a segment
/// handed out by `asReadOnly()` was writable, turning the
/// `UnsupportedOperationException` a mutating access owes them into a silent
/// write. The synthetic 6-slot carrier keeps the same flag at slot 3
/// (`[0]=ptr,[1]=size,[2]=arena,[3]=ro,[4]=alive,[5]=offset` — see
/// `panama::pe_arena_allocate_impl`; `asSlice`/`reinterpret` both propagate it).
///
/// `false` survives only as the fallback for a carrier too short to have the
/// slot, which is what those (2- and 3-field) segments have always been.
/// The backing array of a heap segment, or `None` for a native one.
///
/// `panama::SEG_HEAP_BASE_FIELD` is slot 6 and only a heap-aliasing carrier is
/// that wide, so the width check IS the discriminator — the same one
/// `al_itr_alt_base` uses one crate over, and for the same reason: an ordinary
/// carrier pays one integer compare.
fn p67_segment_heap_base_slot(ctx: &dyn NativeContext, seg: ObjectRef) -> Option<ObjectRef> {
    const SEG_HEAP_BASE_FIELD: usize = 6;
    if ctx.object_num_fields(seg) <= SEG_HEAP_BASE_FIELD {
        return None;
    }
    match ctx.get_field(seg, SEG_HEAP_BASE_FIELD) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

pub(crate) fn p67_segment_is_read_only(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Int(v) = ctx.get_field_by_name(this, "readOnly") {
        return Ok(Some(Value::Int(i32::from(v != 0))));
    }
    if ctx.object_num_fields(this) >= 6 {
        if let Value::Int(v) = ctx.get_field(this, 3) {
            return Ok(Some(Value::Int(i32::from(v != 0))));
        }
    }
    Ok(Some(Value::Int(0)))
}

/// `jdk.internal.foreign.*MemorySegmentImpl.isNative()` — decide from the
/// receiver's OWN runtime class rather than from the class the native happens
/// to be registered on.
///
/// The registration on `AbstractMemorySegmentImpl` also intercepts
/// `HeapMemorySegmentImpl` and its `OfByte`/`OfChar`/… nested subclasses
/// (FACT 1: a native on a concrete/abstract class catches every subclass that
/// doesn't override), so a constant answer keyed to the registration class is
/// wrong for exactly the receivers that reach it through the base.
fn p67_segment_impl_is_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    // Heap segments are the only non-native kind; a mapped segment IS native.
    Ok(Some(Value::Int(i32::from(!name.contains("Heap")))))
}

/// `jdk.internal.foreign.*MemorySegmentImpl.isMapped()` — true only for a
/// segment that came from `FileChannel.map`, i.e. a `MappedMemorySegmentImpl`.
/// See `p67_segment_impl_is_native` for why this cannot be a constant.
fn p67_segment_impl_is_mapped(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    Ok(Some(Value::Int(i32::from(
        name == "jdk/internal/foreign/MappedMemorySegmentImpl",
    ))))
}

/// Upper bound on a `getString` scan, so a segment whose recorded size is
/// implausible cannot turn into an unbounded read.
const P67_MAX_CSTR_LEN: usize = 1 << 20;

/// `MemorySegment.getString(long)` — the UTF-8, NUL-terminated read.
///
/// `MemorySegment` is an interface whose every method is abstract, and the
/// carriers this VM hands out are instances of that interface, so an
/// unregistered method is an `AbstractMethodError: … has no Code attribute`
/// rather than a missing implementation class. `getString` was one:
/// `panama.rs` implements only the JDK-21-preview spelling `getUtf8String`,
/// and that registrar (`register_pe2_string_marshaling`, via
/// `register_pe_panama`) is on the synthetic-JDK arm, so nothing answered
/// `getString` in a shipping binary (regression-suite
/// `RJdkForeign.segmentRoundTrip`).
///
/// Bounds are read from the segment itself through the canonical pair
/// `panama_libffi::segment_address`/`segment_byte_size`, which accept BOTH
/// segment models (`min`/`length` on a real `NativeMemorySegmentImpl`, the
/// `[base@0, size@1, …, offset@5]` synthetic one) — the scan can never run past
/// what the segment claims to own. An offset outside the segment, or a region
/// with no terminator in it, raises `IndexOutOfBoundsException`, which is what
/// the JDK raises; neither is answered with a truncated or empty string.
pub(crate) fn p67_segment_get_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let seg = obj_arg(args, 0)?;
    let offset = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    // GC-safety: `checkValidState()` is Java bytecode and can relocate `seg`.
    // The pin lives inside `p67_segment_check_scope` now, and the `&mut` hands
    // the forwarded reference back here.
    let mut seg = seg;
    p67_segment_check_scope(ctx, &mut seg)?;
    let base = crate::panama_libffi::segment_address(ctx, seg);
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    if offset < 0 || size <= 0 || offset >= size {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!(
                "getString offset {offset} is out of bounds for a segment of {size} bytes"
            )),
        }
        .into());
    }
    let Some(addr) = (base as u64).checked_add(offset as u64).filter(|a| *a != 0) else {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some("getString on a segment with no address".to_string()),
        }
        .into());
    };
    let len = ((size - offset) as usize).min(P67_MAX_CSTR_LEN);
    // SAFETY: `addr` is non-null and `len` is clamped to the bytes the segment
    // itself reports past `offset`, so the scan stays inside the block the
    // arena allocated (and the scope check above proved it is still live).
    let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    let Some(nul) = bytes.iter().position(|b| *b == 0) else {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!(
                "getString found no NUL terminator in the {len} bytes at offset {offset}"
            )),
        }
        .into());
    };
    let text = String::from_utf8_lossy(&bytes[..nul]).into_owned();
    let s = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(s))))
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
    let mut seg = seg;
    p67_segment_check_scope(ctx, &mut seg)?;
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

/// The write half of `AbstractMemorySegmentImpl.checkAccess`.
///
/// `p67_segment_set_width` checked the SCOPE and never the read-only flag, so
/// every write through a native `asReadOnly()` view landed. MEASURED against
/// HotSpot 25.0.4+7, both modes (`apps/probes/FfmSegmentSweep.java`):
///
/// ```text
///   MemorySegment ro = arena.allocate(16).asReadOnly();
///   ro.set(JAVA_INT, 0, 1)
///     HotSpot   IllegalArgumentException: Attempt to write a read-only segment
///     CratonVM  no-throw -- and the following read shows the write LANDED
/// ```
///
/// `asReadOnly()` exists to hand a reference away without handing away the
/// ability to write through it, so a read-only view that writes is not a
/// missing exception: it is the one capability the call was made to remove.
/// `F26` records the same shape one level down ("a copying slice is a wrong
/// capability").
///
/// The heap path has had this check since `heap_segment_check_access`; the
/// raw-address path never did. Unlike the ALIGNMENT rule beside it — which that
/// function's own comment explains cannot be transplanted, because a native
/// segment's real `maxByteAlignment` is not knowable from the carrier — the
/// read-only flag IS on the carrier and `p67_segment_is_read_only` is the
/// registered `isReadOnly()` body, so this reads the single source of truth
/// rather than a second copy of it.
/// `&mut` for the same reason as [`p67_segment_check_scope`], though this one
/// has no Java dispatch of its own today: `p67_segment_is_read_only` only reads
/// fields, so nothing here moves the receiver, and the only allocation is the
/// Rust-side message on the throwing path — which never returns to the caller's
/// reuse. It takes `&mut` anyway because its sole caller reaches it one line
/// after `p67_segment_check_scope`, which DOES relocate, and because a
/// by-value receiver here is the shape the audit is watching for; if this ever
/// grows a dispatch, the callers are already correct.
fn p67_segment_check_writable(
    ctx: &mut dyn NativeContext,
    seg: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let read_only = matches!(
        p67_segment_is_read_only(ctx, &[Value::Object(Some(*seg))])?,
        Some(Value::Int(n)) if n != 0
    );
    if read_only {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Attempt to write a read-only segment".into(),
        }
        .into());
    }
    Ok(())
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
    let mut seg = seg;
    p67_segment_check_scope(ctx, &mut seg)?;
    p67_segment_check_writable(ctx, &mut seg)?;
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
    // Bulk fast path: one contiguous run of native-order 4-byte
    // elements into an `int[]`.
    //
    // The loop below resolves the segment — its fields, its scope, its
    // bounds — once PER ELEMENT and then writes one element through
    // `set_array_element`. That is correct, and it is also why this
    // call measured 15 MB/s where HotSpot does 3.0 GB/s: reading a
    // 2.5 GB weight tensor into the heap would take three minutes.
    // When the run is contiguous, native-order and four bytes wide,
    // resolve the whole range ONCE and hand it to the bulk array
    // write, which the VM implements as a single
    // `copy_nonoverlapping`.
    //
    // Deliberately narrow: a byte, short or long element, a big-endian
    // layout, a destination that is not an `int[]`, or a range that
    // does not resolve in one piece all fall through to the
    // per-element loop unchanged.
    if width == 4 && little_endian && count > 0 {
        let span = count as i64 * 4;
        // `p67_segment_parts` bounds-checks `span` bytes from
        // `src_offset` itself and answers None when they do not fit, so
        // reaching here IS the proof that the whole run is in range.
        if let Some((addr, _size)) = p67_segment_parts(ctx, src, src_offset, span) {
            {
                // SAFETY: `p67_segment_parts` bounds-checked `span`
                // bytes from `addr` against the segment, and
                // `write_int_array_from` bounds-checks the destination
                // before it writes anything.
                let words: &[i32] =
                    unsafe { std::slice::from_raw_parts(addr as *const i32, count) };
                if ctx.write_int_array_from(dst, dst_index, words) {
                    return Ok(None);
                }
            }
        }
    }
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

/// Pins `this` across [`lucene_buffered_checksum_flush_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `this` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub(crate) fn lucene_buffered_checksum_flush(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*this);
    let w5_out = lucene_buffered_checksum_flush_body(ctx, *this);
    *this = ctx.read_native_pin(w5_pin, *this);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

pub(crate) fn lucene_buffered_checksum_flush_body(ctx: &mut dyn NativeContext, this: ObjectRef) {
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
    this: &mut ObjectRef,
    bytes: &[u8],
) {
    let buffer = match ctx.get_field_by_name(*this, "buffer") {
        Value::Object(Some(buffer)) => buffer,
        _ => return,
    };
    let cap = ctx.array_length(buffer);
    let mut upto = ctx
        .get_field_by_name(*this, "upto")
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    if upto.saturating_add(bytes.len()) > cap {
        lucene_buffered_checksum_flush(ctx, this);
        upto = ctx
            .get_field_by_name(*this, "upto")
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
    ctx.set_field_by_name(*this, "upto", Value::Int((upto + bytes.len()) as i32));
}

pub(crate) fn lucene_buffered_checksum_update_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    lucene_buffered_checksum_write(ctx, &mut this, &value.to_le_bytes());
    Ok(None)
}

pub(crate) fn lucene_buffered_checksum_update_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let value = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    lucene_buffered_checksum_write(ctx, &mut this, &value.to_le_bytes());
    Ok(None)
}

pub(crate) fn lucene_buffered_checksum_update_longs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let arr = obj_arg(args, 1)?;
    let mut off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    for _ in 0..len {
        let value = match ctx.get_array_element(arr, off) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        lucene_buffered_checksum_write(ctx, &mut this, &value.to_le_bytes());
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
    // (s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md
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
    // Arena = 2-field (open=0 Int, session=1). See `p67_new_arena`: the session
    // is what gives the arena a lifetime, so `scope()` returns one stable object
    // and `close()` has something to close.
    let arena = "java/lang/foreign/Arena";
    r.register(
        arena,
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| Ok(Some(Value::Object(Some(p67_new_arena(ctx, true)?)))),
    );
    // NON-CLOSEABLE, like `global()` below: `Arena.ofAuto()` is closed by the
    // collector when its segments become unreachable, never by the caller.
    r.register(
        arena,
        "ofAuto",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| Ok(Some(Value::Object(Some(p67_new_arena_kind(ctx, false, false)?)))),
    );
    r.register(
        arena,
        "ofShared",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| Ok(Some(Value::Object(Some(p67_new_arena(ctx, false)?)))),
    );
    // `Arena.global()` IS A SINGLETON. The JDK's is
    // `MemorySessionImpl.GLOBAL_SESSION` -- one object for the life of the VM,
    // and `Arena.global() == Arena.global()` is `true` on HotSpot. This minted
    // a fresh arena on every call, in BOTH modes, so the identity was wrong and
    // so was every per-arena table but the last: state recorded against one
    // global arena was invisible to the next caller's.
    //
    // MEASURED by `apps/probes/FfmCarrierProbe.java`. The memo lives here
    // because this registrar OWNS the slot -- `--dump-native-registry` says
    // `owns_slot=true` here and `panama.rs`'s `register_pe_arena` owns none of
    // the four factories, so the first version of this fix, written there, was
    // inert and the row stayed red while the nine beside it went green.
    r.register(
        arena,
        "global",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| {
            if let Some(existing) = crate::panama::global_arena_handle()
                .and_then(|h| ctx.resolve_global_root(h))
            {
                return Ok(Some(Value::Object(Some(existing))));
            }
            let a = p67_new_arena_kind(ctx, false, false)?;
            // A global root: the global arena outlives every native call and
            // must survive a moving collection.
            let handle = ctx.add_global_root(a);
            match crate::panama::claim_global_arena(handle) {
                None => Ok(Some(Value::Object(Some(a)))),
                // Lost the race -- drop ours and answer with theirs.
                Some(published) => {
                    ctx.remove_global_root(handle);
                    let winner = ctx.resolve_global_root(published).unwrap_or(a);
                    Ok(Some(Value::Object(Some(winner))))
                }
            }
        },
    );
    r.register(
        arena,
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let size = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let segment = p67_arena_segment(ctx, this, size)?;
            Ok(Some(Value::Object(Some(segment))))
        },
    );
    r.register(
        arena,
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let size = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let segment = p67_arena_segment(ctx, this, size)?;
            Ok(Some(Value::Object(Some(segment))))
        },
    );
    // `allocateFrom(String)` allocates REAL memory and writes the string into
    // it. It used to hand back a `p67_arena_segment` — a stand-in whose address
    // is 0 and which `panama_libffi::segment_byte_size` decodes as size 0, so
    // `Arena.allocateFrom("abc").byteSize()` answered 0 where HotSpot answers 4,
    // and the bytes were never written at all. `Arena.allocate` was already
    // routed to the real allocator; this is the sibling that was left behind,
    // and it is the second half of residual 3 in
    // `ffm-elements-spliterator-and-allocatefrom-gaps-20260813`.
    r.register(
        arena,
        "allocateFrom",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        crate::panama::pe_arena_allocate_from_string,
    );
    // The impl class as well: `allocateFrom(String)` is the shape
    // `RJdkForeign.downcall` uses, and it is the one that crashed.
    r.register(
        "jdk/internal/foreign/ArenaImpl",
        "allocateFrom",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        crate::panama::pe_arena_allocate_from_string,
    );
    // BOTH names, and for the reason `register_p67_segment_surface` states a
    // few hundred lines down: native dispatch is keyed on the RECEIVER'S CLASS,
    // and an arena is now stamped `ArenaImpl`. `close()` and `scope()` are real
    // methods on that class, so a registration left only on the interface would
    // lose the dispatch to JDK bytecode reading this VM's field values.
    // Spelled out rather than `[arena, P67_ARENA_IMPL]`: `registrar_drift`
    // resolves the class from the CALL TEXT, and a variable or a const reads to
    // it as no registration at all -- which turned a live drift pair into a
    // "stale baseline" failure. `panama.rs`'s allocator loop spells its classes
    // out for the same reason.
    for arena_cls in [
        "java/lang/foreign/Arena",
        "jdk/internal/foreign/ArenaImpl",
    ] {
        r.register(arena_cls, "close", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            // A non-closeable arena refuses BEFORE anything is torn down --
            // `global()` and `ofAuto()`. See `P67_ARENA_CLOSEABLE` for the
            // measurement; the message is HotSpot's, transcribed.
            let slots = p67_arena_slots(ctx, this);
            if ctx.object_num_fields(this) > slots.closeable
                && matches!(ctx.get_field(this, slots.closeable), Value::Int(0))
            {
                return Err(RuntimeError::UnsupportedOperationException {
                    message: "Attempted to close a non-closeable session".into(),
                }
                .into());
            }
            // Close the arena's session FIRST: a second `close()` must surface
            // the session's IllegalStateException rather than silently
            // re-clearing the flag, and the cleanups have to run while the
            // arena is still open.
            if let Some(mut session) = p67_arena_session(ctx, this) {
                p67_session_just_close(ctx, &mut session)?;
                p67_session_run_close_actions(ctx, session)?;
            }
            ctx.set_field(this, slots.open, Value::Int(0));
            Ok(None)
        });
        r.register(
            arena_cls,
            "scope",
            "()Ljava/lang/foreign/MemorySegment$Scope;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(p67_receiver_session(ctx, this)?))
            },
        );
    }
    let session = "jdk/internal/foreign/MemorySessionImpl";
    r.register(
        session,
        "toMemorySession",
        "(Ljava/lang/foreign/Arena;)Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, args| {
            // Static: arg 0 is the Arena whose session is being unwrapped.
            let arena = obj_arg(args, 0)?;
            Ok(Some(p67_receiver_session(ctx, arena)?))
        },
    );
    r.register(
        session,
        "createConfined",
        "(Ljava/lang/Thread;)Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, args| {
            // Record the confining thread: it is what lets `checkValidStateRaw`
            // raise WrongThreadException for an off-owner access, as the JDK
            // does. Every other factory here yields an unconfined session.
            let owner = match args.first() {
                Some(Value::Object(Some(owner))) => Some(*owner),
                _ => None,
            };
            let owner_pin = owner.map(|owner| ctx.pin_native_root(owner));
            let value = p67_memory_session(ctx)?;
            if let (Value::Object(Some(new_session)), Some(owner), Some(pin)) =
                (value, owner, owner_pin)
            {
                let owner = ctx.read_native_pin(pin, owner);
                let slots = p67_session_slots(ctx, new_session);
                ctx.set_field(new_session, slots.owner, Value::Object(Some(owner)));
            }
            if let Some(pin) = owner_pin {
                ctx.unpin_native_roots(pin);
            }
            Ok(Some(value))
        },
    );
    r.register(
        session,
        "createShared",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx)?)),
    );
    r.register(
        session,
        "createImplicit",
        "(Ljava/lang/ref/Cleaner;)Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx)?)),
    );
    r.register(
        session,
        "createHeap",
        "(Ljava/lang/Object;)Ljdk/internal/foreign/MemorySessionImpl;",
        |ctx, _args| Ok(Some(p67_memory_session(ctx)?)),
    );
    r.register(
        session,
        "addCloseAction",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            if let Some(real) =
                p67_session_delegate(ctx, args, "addCloseAction", "(Ljava/lang/Runnable;)V")
            {
                return real;
            }
            p67_session_add_action_synthetic(ctx, args)
        },
    );
    r.register(
        session,
        "addOrCleanupIfFail",
        "(Ljdk/internal/foreign/MemorySessionImpl$ResourceList$ResourceCleanup;)V",
        |ctx, args| {
            if let Some(real) = p67_session_delegate(
                ctx,
                args,
                "addOrCleanupIfFail",
                "(Ljdk/internal/foreign/MemorySessionImpl$ResourceList$ResourceCleanup;)V",
            ) {
                return real;
            }
            p67_session_add_action_synthetic(ctx, args)
        },
    );
    r.register(
        session,
        "addInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl$ResourceList$ResourceCleanup;)V",
        |ctx, args| {
            if let Some(real) = p67_session_delegate(
                ctx,
                args,
                "addInternal",
                "(Ljdk/internal/foreign/MemorySessionImpl$ResourceList$ResourceCleanup;)V",
            ) {
                return real;
            }
            p67_session_add_action_synthetic(ctx, args)
        },
    );
    r.register(session, "release0", "()V", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "release0", "()V") {
            return real;
        }
        let this = obj_arg(args, 0)?;
        p67_session_release(ctx, this);
        Ok(None)
    });
    r.register(session, "acquire0", "()V", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "acquire0", "()V") {
            return real;
        }
        let mut this = obj_arg(args, 0)?;
        p67_session_acquire(ctx, &mut this)?;
        Ok(None)
    });
    r.register(
        session,
        "whileAlive",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            if let Some(real) =
                p67_session_delegate(ctx, args, "whileAlive", "(Ljava/lang/Runnable;)V")
            {
                return real;
            }
            let mut this = obj_arg(args, 0)?;
            let action = obj_arg(args, 1)?;
            // The acquire count is what keeps the session alive across the
            // callback: a nested `close()` sees a non-zero count and refuses,
            // exactly as the JDK's `whileAlive` does. Released even when the
            // action throws, or the session could never be closed afterwards.
            p67_session_acquire(ctx, &mut this)?;
            let this_pin = ctx.pin_native_root(this);
            let result = ctx.invoke_virtual(action, "run", "()V", &[]);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            p67_session_release(ctx, this);
            result?;
            Ok(None)
        },
    );
    r.register(
        session,
        "ownerThread",
        "()Ljava/lang/Thread;",
        |ctx, args| {
            if let Some(real) =
                p67_session_delegate(ctx, args, "ownerThread", "()Ljava/lang/Thread;")
            {
                return real;
            }
            let this = obj_arg(args, 0)?;
            if p67_session_modelled(ctx, this) {
                let slots = p67_session_slots(ctx, this);
                if let Value::Object(Some(owner)) = ctx.get_field(this, slots.owner) {
                    return Ok(Some(Value::Object(Some(owner))));
                }
            }
            // Unconfined session: keep the historical "current thread" answer
            // rather than the JDK's null, which callers here do not expect.
            Ok(Some(Value::Object(Some(ctx.current_thread_object()))))
        },
    );
    r.register(
        session,
        "isAccessibleBy",
        "(Ljava/lang/Thread;)Z",
        |ctx, args| {
            if let Some(real) =
                p67_session_delegate(ctx, args, "isAccessibleBy", "(Ljava/lang/Thread;)Z")
            {
                return real;
            }
            let this = obj_arg(args, 0)?;
            if p67_session_modelled(ctx, this) {
                let slots = p67_session_slots(ctx, this);
                if let Value::Object(Some(owner)) = ctx.get_field(this, slots.owner) {
                    let accessible =
                        matches!(args.get(1), Some(Value::Object(Some(t))) if *t == owner);
                    return Ok(Some(Value::Int(i32::from(accessible))));
                }
            }
            Ok(Some(Value::Int(1)))
        },
    );
    r.register(session, "isAlive", "()Z", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "isAlive", "()Z") {
            return real;
        }
        let this = obj_arg(args, 0)?;
        let alive = !p67_session_modelled(ctx, this) || p67_session_state(ctx, this) == 1;
        Ok(Some(Value::Int(i32::from(alive))))
    });
    r.register(session, "checkValidStateRaw", "()V", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "checkValidStateRaw", "()V") {
            return real;
        }
        let mut this = obj_arg(args, 0)?;
        p67_session_check_valid(ctx, &mut this)?;
        Ok(None)
    });
    r.register(session, "checkValidState", "()V", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "checkValidState", "()V") {
            return real;
        }
        let mut this = obj_arg(args, 0)?;
        p67_session_check_valid(ctx, &mut this)?;
        Ok(None)
    });
    r.register(
        session,
        "checkValidState",
        "(Ljava/lang/foreign/MemorySegment;)V",
        |ctx, args| {
            // W7-89 (found by W7-86-static-native-arity.md §4.1 row 6). This
            // overload is `public STATIC void checkValidState(MemorySegment)`,
            // so `args[0]` is the SEGMENT, not a session receiver — the comment
            // that used to sit here ("validity is a property of the session
            // receiver") described the zero-argument instance overload above.
            // The body handed the segment to `p67_session_check_valid`, which
            // begins `if !p67_session_modelled(session) { return Ok(()) }`; a
            // segment is not a modelled session, so the check returned `Ok(())`
            // for every input. It validated nothing.
            //
            // The JDK's own body is
            //   ((AbstractMemorySegmentImpl) segment).sessionImpl().checkValidState();
            // i.e. resolve the segment's session, then check THAT — which is
            // exactly `p67_segment_check_scope`.
            let mut segment = obj_arg(args, 0)?;
            p67_segment_check_scope(ctx, &mut segment)?;
            Ok(None)
        },
    );
    r.register(session, "isCloseable", "()Z", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "isCloseable", "()Z") {
            return real;
        }
        Ok(Some(Value::Int(1)))
    });
    r.register(session, "close", "()V", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "close", "()V") {
            return real;
        }
        let mut this = obj_arg(args, 0)?;
        p67_session_just_close(ctx, &mut this)?;
        p67_session_run_close_actions(ctx, this)?;
        Ok(None)
    });
    r.register(session, "justClose", "()V", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "justClose", "()V") {
            return real;
        }
        let mut this = obj_arg(args, 0)?;
        p67_session_just_close(ctx, &mut this)?;
        Ok(None)
    });

    // The `MemorySegment` surface, once per receiver class. See
    // `panama::CRATON_SEGMENT_CLASS` for why there are two names and what the
    // second one is: native dispatch is keyed on the RECEIVER's class, and
    // since 2026-08-22 a CratonVM-minted segment is stamped with a concrete
    // class of its own rather than with the interface.
    for ms in [
        crate::panama::PE_SEGMENT_INTERFACE,
        crate::panama::CRATON_SEGMENT_CLASS,
    ] {
        register_p67_segment_surface(r, ms);
    }
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
        // `isNative`/`isMapped` were a constant TRUE for all three classes,
        // which is right for only two of the six answers — a NATIVE segment is
        // not mapped, and `AbstractMemorySegmentImpl` is the shared base of the
        // HEAP impls too, so a blanket TRUE there reported every heap segment
        // as both native and mapped. Answer from the receiver's own runtime
        // class instead (see `p67_segment_impl_is_native`).
        r.register(
            ms_impl,
            "getString",
            "(J)Ljava/lang/String;",
            p67_segment_get_string,
        );
        r.register(ms_impl, "isNative", "()Z", p67_segment_impl_is_native);
        r.register(ms_impl, "isMapped", "()Z", p67_segment_impl_is_mapped);
        // Was a constant `false`, which shadowed the real, concrete
        // `AbstractMemorySegmentImpl.isReadOnly()` — see the helper's doc.
        r.register(ms_impl, "isReadOnly", "()Z", p67_segment_is_read_only);
        r.register(
            ms_impl,
            "scope",
            "()Ljava/lang/foreign/MemorySegment$Scope;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(p67_receiver_session(ctx, this)?))
            },
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
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfByte", 1, 1)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_BOOLEAN",
        "Ljava/lang/foreign/ValueLayout$OfBoolean;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfBoolean", 1, 1)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_CHAR",
        "Ljava/lang/foreign/ValueLayout$OfChar;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfChar", 2, 2)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_SHORT",
        "Ljava/lang/foreign/ValueLayout$OfShort;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfShort", 2, 2)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_INT",
        "Ljava/lang/foreign/ValueLayout$OfInt;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfInt", 4, 4)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_LONG",
        "Ljava/lang/foreign/ValueLayout$OfLong;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfLong", 8, 8)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_FLOAT",
        "Ljava/lang/foreign/ValueLayout$OfFloat;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfFloat", 4, 4)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "JAVA_DOUBLE",
        "Ljava/lang/foreign/ValueLayout$OfDouble;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/ValueLayout$OfDouble", 8, 8)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        vl,
        "ADDRESS",
        "Ljava/lang/foreign/AddressLayout;",
        |ctx, _args| {
            let obj = p67_layout_object(ctx, "java/lang/foreign/AddressLayout", 8, 8)?;
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
            |ctx, args| Ok(Some(p67_var_handle(ctx, args)?)),
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
            p67_layout_with_byte_alignment,
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
            p67_layout_with_byte_alignment,
        );
        r.register(
            class,
            "withByteAlignment",
            "(J)Ljava/lang/foreign/ValueLayout;",
            p67_layout_with_byte_alignment,
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
        p67_layout_with_byte_alignment,
    );
    r.register(
        "java/lang/foreign/ValueLayout",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
        p67_layout_with_byte_alignment,
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
        p67_layout_with_byte_alignment,
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
            p67_layout_with_byte_alignment,
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
            p67_layout_with_byte_alignment,
        );
        r.register(
            class,
            "withByteAlignment",
            "(J)Ljava/lang/foreign/ValueLayout;",
            p67_layout_with_byte_alignment,
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
        // The four the interface loop has and this one did not. A value layout
        // is minted as the impl class since 2026-08-29 and native dispatch is
        // keyed on the receiver's class, so without these the JDK's own
        // bytecode -- reading fields this VM does not populate -- serves them.
        r.register(class, "byteSize", "()J", p67_layout_byte_size);
        r.register(class, "byteAlignment", "()J", p67_layout_byte_alignment);
        r.register(
            class,
            "byteOffset",
            "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
            p67_layout_byte_offset,
        );
        r.register(
            class,
            "varHandle",
            "()Ljava/lang/invoke/VarHandle;",
            |ctx, args| Ok(Some(p67_var_handle(ctx, args)?)),
        );
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
            // THE JDK DOES NOT AUTO-PAD A STRUCT, AND DOES NOT ROUND THE TOTAL
            // UP TO THE ALIGNMENT (F16, 2026-08-13). Both are measured, and
            // this VM used to do both. Verbatim from the oracle's own source,
            // jdk25src/java.base/jdk/internal/foreign/layout/StructLayoutImpl.java:
            //
            //     public static StructLayout of(List<MemoryLayout> elements) {
            //         long size = 0;
            //         long align = 1;
            //         for (MemoryLayout elem : elements) {
            //             if (size % elem.byteAlignment() != 0) {
            //                 throw new IllegalArgumentException(
            //                     "Invalid alignment constraint for member layout: " + elem);
            //             }
            //             size = Math.addExact(size, elem.byteSize());
            //             align = Math.max(align, elem.byteAlignment());
            //         }
            //         ...
            //     }
            //
            // and confirmed by running it (Microsoft build 25.0.3+9-LTS):
            //
            //     structLayout(JAVA_BYTE, JAVA_INT)  -> IAE: Invalid alignment
            //                                    constraint for member layout: i4
            //     structLayout(JAVA_INT, JAVA_LONG)  -> IAE: ... : j8
            //     structLayout(JAVA_BYTE, paddingLayout(3), JAVA_INT) -> 8  align 4
            //     structLayout(JAVA_LONG, JAVA_INT)                   -> 12 align 8
            //     structLayout(JAVA_INT,  JAVA_BYTE)                  -> 5  align 4
            //     structLayout()                                      -> 0  align 1
            //
            // `structLayout(JAVA_LONG, JAVA_INT) == 12` is the one that the
            // old `total_size = align_up(offset, max_align)` got wrong even
            // for a call the JDK ACCEPTS: it answered 16. The padding is the
            // caller's job in both directions.
            let members = match args.first() {
                Some(Value::Object(Some(arr))) => *arr,
                _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
            };
            let count = ctx.array_length(members);
            let mut size = 0_i64;
            let mut max_align = 1_i64;
            for i in 0..count {
                // A NULL member is a `NullPointerException`, not a member to
                // skip. MEASURED 2026-08-16: `structLayout((MemoryLayout) null)`
                // and `structLayout(JAVA_INT, null)` are both
                // `NullPointerException` with a null message
                // (`Objects.requireNonNull` inside `MemoryLayout.structLayout`).
                // The `continue` this replaces answered a layout that was one
                // member short — `structLayout(JAVA_LONG, null)` was `j8`,
                // byteSize 8, with nothing to say a member had gone missing.
                let member = match ctx.get_array_element(members, i) {
                    Value::Object(Some(obj)) => obj,
                    _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
                };
                let Some((member_size, member_align)) = p67_member_size_align(ctx, member) else {
                    // Not a layout carrier. Name it rather than defaulting to
                    // a plausible zero — see the banner at
                    // `p67_member_size_align`.
                    let cls = ctx
                        .class_name_of_id(ctx.class_id_of_object(member))
                        .unwrap_or_else(|| "<unknown>".to_string());
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!(
                            "MemoryLayout.structLayout: member {i} is not a memory layout \
                             (class {cls}, slot 0 = {:?})",
                            ctx.get_field(member, 0)
                        ),
                    }
                    .into());
                };
                if member_align <= 0 || size % member_align != 0 {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!(
                            "Invalid alignment constraint for member layout: {}",
                            p67_layout_render(ctx, member)
                        ),
                    }
                    .into());
                }
                let Some(next) = size.checked_add(member_size.max(0)) else {
                    // MEASURED 2026-08-16, and NOT what this said before.
                    //
                    // The JDK does use `Math.addExact` here, but it does not
                    // let the `ArithmeticException` out: `AbstractLayout`
                    // catches it and rethrows. Oracle, with
                    // `big = sequenceLayout(Long.MAX_VALUE, JAVA_BYTE)`:
                    //
                    //     structLayout(big, JAVA_BYTE) -> IllegalArgumentException:
                    //                          Layout size exceeds Long.MAX_VALUE
                    //     structLayout(big, big)       -> the same
                    //
                    // "an overflow there is an ArithmeticException" was a
                    // PREDICTION read off the `Math.addExact` call, and it is
                    // the wrong exception CLASS — a caller catching
                    // `IllegalArgumentException`, which is what every other
                    // refusal in this factory throws, would not have caught it.
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "Layout size exceeds Long.MAX_VALUE".to_string(),
                    }
                    .into());
                };
                size = next;
                max_align = max_align.max(member_align);
            }
            let members_pin = ctx.pin_native_root(members);
            let obj = try_alloc_concurrent_synthetic(ctx, "jdk/internal/foreign/layout/StructLayoutImpl", 4)?;
            let slots = p67_layout_slots_for_mint(ctx, obj, 3);
            let members = ctx.read_native_pin(members_pin, members);
            ctx.set_field(obj, slots.byte_size, Value::Long(size));
            ctx.set_field(obj, slots.byte_alignment, Value::Long(max_align));
            ctx.set_field(obj, slots.payload, Value::Object(Some(members)));
            ctx.set_field(obj, slots.name, Value::Object(None));
            ctx.unpin_native_roots(members_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // The three carriers below used to be ONE slot wide, holding the
    // constructor's own argument — the element COUNT for a sequence, a
    // hard-coded 0 for a union — while every reader in the tree expects the
    // `[0]=byteSize, [1]=byteAlignment` prefix `p67_layout_object` defines. So
    // `MemoryLayout.sequenceLayout(4, JAVA_INT).byteSize()` did not merely
    // answer wrong, it answered `AbstractMethodError: MemoryLayout.byteSize()
    // has no Code attribute` — no `byteSize` was registered for these classes
    // at all, and a one-slot object could not have served one.
    r.register(
        ml,
        "sequenceLayout",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/SequenceLayout;",
        |ctx, args| {
            // The carrier used to be ONE slot holding the element COUNT, which
            // no reader in this file understands: `byteSize()` reads slot 0 as
            // a size, so `sequenceLayout(4, JAVA_INT).byteSize()` would have
            // answered 4 instead of 16 — and did not even get that far,
            // because `byteSize` was registered on no sequence class at all
            // (`AbstractMethodError: MemoryLayout.byteSize()J has no Code
            // attribute`, which is what `arena.allocate(seq)` hit through the
            // real `SegmentAllocator.allocate(MemoryLayout)` default method).
            //
            // It now carries the same 4-slot shape as `structLayout`:
            // `[0] byteSize, [1] byteAlignment, [2] ELEMENT layout, [3] name`.
            // Slot 2 differs in meaning from a group layout's member ARRAY,
            // and the two are told apart by the receiver's class — see
            // `p67_sequence_element_layout`.
            let count = match args.first() {
                Some(Value::Long(v)) => *v,
                Some(Value::Int(v)) => *v as i64,
                _ => 0,
            };
            // Measured on the oracle:
            //   sequenceLayout(10, JAVA_INT) -> byteSize=40 align=4   (agrees)
            //   sequenceLayout(0,  JAVA_INT) -> byteSize=0  align=4   (agrees)
            //   sequenceLayout(-1, JAVA_INT) -> IllegalArgumentException:
            //                        The provided elementCount is negative: -1
            // The negative case was accepted here and answered a negative
            // byteSize.
            if count < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("The provided elementCount is negative: {count}"),
                }
                .into());
            }
            // The remaining three refusals, in the ORDER the oracle applies
            // them. MEASURED 2026-08-16 (`FfmProbe4`, `P5`):
            //
            // | call | oracle |
            // |---|---|
            // | `sequenceLayout(-1, null)` | IAE `The provided elementCount is negative: -1` |
            // | `sequenceLayout(4, null)` | `NullPointerException` |
            // | `sequenceLayout(0, structLayout(JAVA_INT, JAVA_BYTE))` | IAE `Element layout size is not multiple of alignment` |
            // | `sequenceLayout(2, JAVA_INT.withByteAlignment(8))` | IAE, same message |
            // | `sequenceLayout(Long.MAX_VALUE, JAVA_INT)` | IAE `Layout size exceeds Long.MAX_VALUE` |
            // | `sequenceLayout(Long.MAX_VALUE, JAVA_BYTE)` | 9223372036854775807 |
            //
            // The negative-count check above wins even over a null element
            // (`sequenceLayout(-1, null)` is the count message, not an NPE),
            // which is why it stays first.
            let element = match args.get(1) {
                Some(Value::Object(Some(e))) => *e,
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            let (elem_size, elem_align) = p67_layout_size_align(ctx, element);
            // An element whose own size is not a whole number of its own
            // alignment cannot tile, and the JDK refuses it AT THE FACTORY —
            // before the count is even multiplied in, which is why count 0
            // refuses too. Without this,
            // `sequenceLayout(2, structLayout(JAVA_INT, JAVA_BYTE))` answered a
            // 10-byte layout with alignment 4, whose second element starts at
            // byte 5. `pe_segment_spliterator` already carries this exact check
            // and this exact message for a segment's element layout; the
            // factory is where the oracle puts it.
            if elem_align > 0 && elem_size % elem_align != 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Element layout size is not multiple of alignment".to_string(),
                }
                .into());
            }
            // `SequenceLayoutImpl`'s constructor is
            // `Math.multiplyExact(elemCount, elementLayout.byteSize())`, but
            // the `ArithmeticException` does not escape — the JDK rethrows it.
            // MEASURED: `sequenceLayout(Long.MAX_VALUE, JAVA_INT)` is
            // `IllegalArgumentException: Layout size exceeds Long.MAX_VALUE`.
            // "an overflow is an ArithmeticException" was a PREDICTION read
            // off the `multiplyExact` call and is the wrong exception class.
            let Some(total) = count.checked_mul(elem_size) else {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Layout size exceeds Long.MAX_VALUE".to_string(),
                }
                .into());
            };
            // FOUR slots, not six. A six-slot carrier that parked the element
            // layout at slot 4 and the count at slot 5 (and the endian flag at
            // slot 2) is the OTHER encoding this file spent F16 collapsing:
            // slot 2 is the payload for every layout here, and
            // `p67_sequence_element_layout` — the single reader both the
            // `byteOffset` and the `varHandle` path walks go through — reads a
            // sequence's element from slot 2. Six slots would have left the
            // walk reading `Int(littleEndian)` as a layout.
            //
            // THE COUNT IS STORED, AT SLOT 4, AND IT IS NOT DERIVABLE.
            //
            // This carrier used to be exactly four slots on the reasoning that
            // `elementCount()` could divide `byteSize` by the element size. The
            // oracle falsifies it: an element layout may have byteSize ZERO,
            // and then the total is 0 for every count. MEASURED —
            // `sequenceLayout(3, structLayout()).byteSize()` is 0 and its
            // `elementCount()` is **3**; `sequenceLayout(2, sequenceLayout(0,
            // JAVA_INT)).elementCount()` is 2. The division answered 0 for all
            // of them (it is guarded against a divide-by-zero, so it was a
            // quiet wrong number rather than a crash).
            //
            // Slot 4 is a SEQUENCE-ONLY EXTENSION and changes nothing about the
            // shared prefix: `[0]=byteSize, [1]=byteAlignment, [2]=payload,
            // [3]=name` is still what `p67_layout_size_align`,
            // `p67_layout_name` and `p67_sequence_element_layout` read, and
            // slot 4 is read by `elementCount()` alone. It is NOT the six-slot
            // carrier F16 removed: that one put the ELEMENT at slot 4, where
            // the walk expects it at slot 2.
            let element_pin = ctx.pin_native_root(element);
            let obj = try_alloc_concurrent_synthetic(ctx, "jdk/internal/foreign/layout/SequenceLayoutImpl", 5)?;
            let slots = p67_layout_slots_for_mint(ctx, obj, 3);
            ctx.set_field(obj, slots.byte_size, Value::Long(total));
            ctx.set_field(obj, slots.byte_alignment, Value::Long(elem_align.max(1)));
            let element = ctx.read_native_pin(element_pin, element);
            ctx.set_field(obj, slots.payload, Value::Object(Some(element)));
            ctx.set_field(obj, slots.name, Value::Object(None));
            ctx.set_field(obj, slots.element_count, Value::Long(count));
            ctx.unpin_native_roots(element_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ml,
        "unionLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;",
        |ctx, args| {
            // This used to DISCARD its members and answer a one-slot carrier
            // holding `Long(0)` — `unionLayout(JAVA_INT, JAVA_LONG).byteSize()`
            // was 0, and `UnionLayout` had no `byteSize` registration to read
            // it with anyway. Measured on the oracle, and matching
            // `UnionLayoutImpl.of` (size = max, align = max, and NO alignment
            // constraint, because every union member sits at offset 0):
            //
            //     unionLayout(JAVA_INT, JAVA_LONG) -> byteSize=8 align=8
            //     unionLayout(JAVA_BYTE, JAVA_INT) -> byteSize=4 align=4
            //
            // The size is NOT rounded up to the alignment. `UnionLayoutImpl.of`
            // is `size = Math.max(size, elem.byteSize())` and nothing else, so
            // `unionLayout(structLayout(JAVA_INT, JAVA_BYTE))` is 5 — the same
            // "the JDK never pads for you" rule `structLayout` above is built
            // on (F16). Rounding agrees with the oracle only on the two rows
            // above, where every member is already a power-of-two value layout.
            let members = match args.first() {
                Some(Value::Object(Some(arr))) => *arr,
                _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
            };
            let count = ctx.array_length(members);
            let mut size = 0_i64;
            let mut max_align = 1_i64;
            for i in 0..count {
                let Some(member) = (match ctx.get_array_element(members, i) {
                    Value::Object(Some(obj)) => Some(obj),
                    _ => None,
                }) else {
                    // MEASURED: `unionLayout(JAVA_INT, null)` is a
                    // `NullPointerException`, the same rule `structLayout`
                    // above now follows. Skipping the member answered a union
                    // sized by the members that happened to be non-null.
                    return Err(RuntimeError::NullPointerException { message: None }.into());
                };
                let Some((member_size, member_align)) = p67_member_size_align(ctx, member) else {
                    let cls = ctx
                        .class_name_of_id(ctx.class_id_of_object(member))
                        .unwrap_or_else(|| "<unknown>".to_string());
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!(
                            "MemoryLayout.unionLayout: member {i} is not a memory layout \
                             (class {cls}, slot 0 = {:?})",
                            ctx.get_field(member, 0)
                        ),
                    }
                    .into());
                };
                size = size.max(member_size);
                max_align = max_align.max(member_align);
            }
            // Four slots with the MEMBERS at slot 2, not `p67_layout_object`'s
            // value-layout shape (whose slot 2 is the endian flag): a union is
            // a group layout, `p67_layout_named_member` resolves
            // `groupElement(name)` against slot 2, and `memberLayouts()` reads
            // it. Minting one through `p67_layout_object` answers the right
            // byteSize and then loses every member.
            let members_pin = ctx.pin_native_root(members);
            let obj = try_alloc_concurrent_synthetic(ctx, "jdk/internal/foreign/layout/UnionLayoutImpl", 4)?;
            let slots = p67_layout_slots_for_mint(ctx, obj, 3);
            let members = ctx.read_native_pin(members_pin, members);
            ctx.set_field(obj, slots.byte_size, Value::Long(size));
            ctx.set_field(obj, slots.byte_alignment, Value::Long(max_align));
            ctx.set_field(obj, slots.payload, Value::Object(Some(members)));
            ctx.set_field(obj, slots.name, Value::Object(None));
            ctx.unpin_native_roots(members_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ml,
        "paddingLayout",
        "(J)Ljava/lang/foreign/PaddingLayout;",
        |ctx, args| {
            // A PADDING LAYOUT'S ALIGNMENT IS ALWAYS 1, and its carrier must
            // have the same four-slot head as every other layout here. The
            // one-slot carrier this used to mint was the reason `structLayout`
            // got the CORRECT JDK idiom wrong: with no slot 1 to read, the old
            // member decode fell back to "alignment = size", so a
            // `paddingLayout(3)` claimed alignment 3 and
            //
            //     structLayout(JAVA_BYTE, paddingLayout(3), JAVA_INT)
            //
            // — the padded form the JDK REQUIRES here — answered 12 where the
            // oracle answers 8. Measured: `paddingLayout(3)` is byteSize=3,
            // byteAlignment=1, and prints as `x3`.
            let size = match args.first() {
                Some(Value::Long(v)) => *v,
                Some(Value::Int(v)) => *v as i64,
                _ => 0,
            };
            // `MemoryLayout.paddingLayout` rejects a non-positive size at the
            // FACTORY: `IllegalArgumentException: Invalid byte size: 0`. Letting
            // a zero-size layout through produced one that every consumer had
            // to re-check — `spliterator(paddingLayout(0))` reported the failure
            // one call later and with a different message than HotSpot's.
            if size <= 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("Invalid byte size: {}", size),
                }
                .into());
            }
            // Built here rather than through `p67_layout_object` for the same
            // reason the group layouts are: slot 2 is the PAYLOAD slot in the
            // one carrier encoding (member array / element layout / null), and
            // `p67_layout_object` stamps the VALUE-layout endian flag there.
            // Padding has no payload, so the slot is explicitly null rather
            // than an `Int` that a payload reader could mistake for one.
            let obj = try_alloc_concurrent_synthetic(ctx, "jdk/internal/foreign/layout/PaddingLayoutImpl", 4)?;
            let slots = p67_layout_slots_for_mint(ctx, obj, 3);
            ctx.set_field(obj, slots.byte_size, Value::Long(size));
            // Padding has no alignment constraint of its own — the JDK's
            // `PaddingLayoutImpl` is byte-aligned. This is the slot whose
            // absence made `structLayout(JAVA_BYTE, paddingLayout(3), JAVA_INT)`
            // answer 12 where the oracle answers 8: with no slot 1 to read, the
            // member decode fell back to "alignment = size".
            ctx.set_field(obj, slots.byte_alignment, Value::Long(1));
            ctx.set_field(obj, slots.payload, Value::Object(None));
            ctx.set_field(obj, slots.name, Value::Object(None));
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
    // The interface-level fallback for a receiver whose concrete layout class
    // is not one of the ones registered below. `panama.rs` used to supply this
    // row, decoding slot 0 as `Int(kind)` and slot 1 as the size; it is
    // deleted, and this is its replacement in the one encoding (F16,
    // 2026-08-13). Registrations here are keyed by RECEIVER class, so the
    // per-class rows are what normally answer — this is the safety net, not
    // the main path.
    r.register(ml, "byteSize", "()J", p67_layout_byte_size);
    r.register(ml, "byteAlignment", "()J", p67_layout_byte_alignment);

    // `byteOffset(PathElement...)` on every layout carrier this file mints.
    //
    // Registered per RECEIVER class, not only on the `MemoryLayout` interface,
    // because that is how a registration reaches one of these objects: the
    // carriers are instances of the interface the factory names
    // (`java/lang/foreign/StructLayout`, `…$OfInt`, …), and the interface
    // method the call site resolved is only what the AbstractMethodError gets
    // NAMED after. `MemorySegment.get`/`byteSize` above are registered exactly
    // this way, and the value layouts are included so a zero-length path on a
    // value layout answers 0 instead of raising.
    const BYTE_OFFSET_DESC: &str = "([Ljava/lang/foreign/MemoryLayout$PathElement;)J";
    for layout_class in [
        ml,
        "java/lang/foreign/StructLayout",
        "java/lang/foreign/GroupLayout",
        "java/lang/foreign/UnionLayout",
        "java/lang/foreign/SequenceLayout",
        "java/lang/foreign/PaddingLayout",
        // The real classes these carriers are minted as since 2026-08-30.
        // Native dispatch is keyed on the receiver's CLASS, so a registration
        // left only on the interface hands the call to JDK bytecode reading
        // fields this VM does not populate.
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "java/lang/foreign/AddressLayout",
        "java/lang/foreign/ValueLayout",
        "java/lang/foreign/ValueLayout$OfByte",
        "java/lang/foreign/ValueLayout$OfBoolean",
        "java/lang/foreign/ValueLayout$OfChar",
        "java/lang/foreign/ValueLayout$OfShort",
        "java/lang/foreign/ValueLayout$OfInt",
        "java/lang/foreign/ValueLayout$OfLong",
        "java/lang/foreign/ValueLayout$OfFloat",
        "java/lang/foreign/ValueLayout$OfDouble",
    ] {
        r.register(
            layout_class,
            "byteOffset",
            BYTE_OFFSET_DESC,
            p67_layout_byte_offset,
        );
    }

    // The sequence layout's own accessors. Its carrier now has the same
    // `[byteSize, byteAlignment, …, name]` head as every other layout here, so
    // it can share the same four readers, and `varHandle` reaches the shared
    // path walk (which is what turns `sequenceElement()` into an index
    // coordinate instead of addressing the sequence itself).
    let seq_layout = "java/lang/foreign/SequenceLayout";
    r.register(seq_layout, "byteSize", "()J", p67_layout_byte_size);
    r.register(
        seq_layout,
        "byteAlignment",
        "()J",
        p67_layout_byte_alignment,
    );
    r.register(
        seq_layout,
        "name",
        "()Ljava/util/Optional;",
        p67_layout_name,
    );
    r.register(
        seq_layout,
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
        p67_layout_with_name,
    );
    r.register(
        seq_layout,
        "varHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
        p67_memory_layout_var_handle,
    );
    r.register(
        seq_layout,
        "elementLayout",
        "()Ljava/lang/foreign/MemoryLayout;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, p67_layout_slots(ctx, this).payload)))
        },
    );
    // `elementCount()` reads the STORED count (slot 4), falling back to the
    // division only for a carrier minted before slot 4 existed.
    //
    // The division is not equivalent, and the oracle says so: an element with
    // byteSize 0 makes every total 0, and `sequenceLayout(3, structLayout())
    // .elementCount()` is **3** on HotSpot where the division answers 0
    // (MEASURED, `FfmProbe4` N7/N10/N12). The fallback is kept — and only the
    // fallback divides — so a four-slot sequence carrier from any other mint
    // still answers what it used to instead of reading past its own end.
    r.register(seq_layout, "elementCount", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let slots = p67_layout_slots(ctx, this);
        if ctx.object_num_fields(this) > slots.element_count {
            if let Value::Long(count) = ctx.get_field(this, slots.element_count) {
                return Ok(Some(Value::Long(count)));
            }
        }
        let total = p67_layout_size_of(ctx, this);
        let elem = match ctx.get_field(this, slots.payload) {
            Value::Object(Some(e)) => p67_layout_size_of(ctx, e),
            _ => 0,
        };
        Ok(Some(Value::Long(if elem > 0 { total / elem } else { 0 })))
    });
    // The same five on the class a sequence is actually minted as. The loops
    // above cover its `byteSize`, `byteAlignment` and `byteOffset`; these are
    // the ones registered against the bare `seq_layout` constant.
    // `toString()` on every class a layout is now minted as.
    //
    // It was registered on nothing at all. While the carrier was the INTERFACE
    // that fell through to `Object.toString` and printed an identity string --
    // wrong against HotSpot's `[3:i4]`, and untested. On the real class the
    // JDK's own `toString` runs instead and dereferences `carrier`, a field
    // this VM does not populate: NPE. `p67_layout_render` is the rendering the
    // oracle produces, and until now it was only ever used inside error
    // messages.
    for impl_class in [
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
    ] {
        r.register(
            impl_class,
            "toString",
            "()Ljava/lang/String;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let rendered = p67_layout_render(ctx, this);
                Ok(Some(Value::Object(Some(ctx.create_string(&rendered)))))
            },
        );
    }

    // Padding's two, for the same reason. The census diff (impl class against
    // its interface) is what named them; the registrars are too spread out to
    // read for this.
    let pad_impl = "jdk/internal/foreign/layout/PaddingLayoutImpl";
    r.register(pad_impl, "name", "()Ljava/util/Optional;", p67_layout_name);
    r.register(
        pad_impl,
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
        p67_layout_with_name,
    );
    let seq_impl = "jdk/internal/foreign/layout/SequenceLayoutImpl";
    r.register(
        seq_impl,
        "name",
        "()Ljava/util/Optional;",
        p67_layout_name,
    );
    r.register(
        seq_impl,
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
        p67_layout_with_name,
    );
    r.register(
        seq_impl,
        "varHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
        p67_memory_layout_var_handle,
    );
    r.register(
        seq_impl,
        "elementLayout",
        "()Ljava/lang/foreign/MemoryLayout;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, p67_layout_slots(ctx, this).payload)))
        },
    );
    r.register(seq_impl, "elementCount", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let slots = p67_layout_slots(ctx, this);
        if ctx.object_num_fields(this) > slots.element_count {
            if let Value::Long(count) = ctx.get_field(this, slots.element_count) {
                return Ok(Some(Value::Long(count)));
            }
        }
        let total = p67_layout_size_of(ctx, this);
        let elem = match ctx.get_field(this, slots.payload) {
            Value::Object(Some(e)) => p67_layout_size_of(ctx, e),
            _ => 0,
        };
        Ok(Some(Value::Long(if elem > 0 { total / elem } else { 0 })))
    });
    // The struct/group carriers reach the path walk through the same
    // registration; without it a `struct.varHandle(groupElement("c"))` would
    // have resolved to the abstract interface declaration.
    for group_class in [
        "java/lang/foreign/StructLayout",
        "java/lang/foreign/GroupLayout",
        "java/lang/foreign/UnionLayout",
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "jdk/internal/foreign/layout/UnionLayoutImpl",
    ] {
        r.register(
            group_class,
            "varHandle",
            "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
            p67_memory_layout_var_handle,
        );
    }

    // `byteSize()`/`byteAlignment()` on the INTERFACE, not only on the
    // concrete layout classes.
    //
    // CratonVM fabricates every `java.lang.foreign` object as an instance of
    // the interface it implements (`ValueLayout.JAVA_INT.getClass()` is
    // `java.lang.foreign.ValueLayout$OfInt`, where HotSpot has
    // `jdk.internal.foreign.layout.ValueLayouts$OfIntImpl`), and an
    // `invokeinterface MemoryLayout.byteSize()` against such a carrier resolves
    // to the interface method — not to the per-class registration. Real JDK
    // bytecode reaches it that way constantly: `SegmentAllocator.allocateFrom
    // (JAVA_INT, 1, 2, 3)` failed with `MemoryLayout.byteSize() has no Code
    // attribute` even though `JAVA_INT.byteSize()` answered 4 one line earlier.
    //
    // Safe to register on the interface for the same reason the rest of this
    // file does: a non-static interface method's native only reaches receivers
    // whose class IS the interface, i.e. exactly these carriers. The uniform
    // `[0]=byteSize, [1]=byteAlignment` prefix is what makes one registration
    // serve all of them.
    // (The two `ml` rows themselves are registered once, further up, with the
    // "interface-level fallback" note — same function, so a second identical
    // row would only be noise.)
    for layout_class in [
        "java/lang/foreign/SequenceLayout",
        "java/lang/foreign/PaddingLayout",
        "java/lang/foreign/UnionLayout",
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
    ] {
        r.register(layout_class, "byteSize", "()J", p67_layout_byte_size);
        r.register(
            layout_class,
            "byteAlignment",
            "()J",
            p67_layout_byte_alignment,
        );
    }

    // `SequenceLayout.elementLayout()`/`elementCount()` are registered ONCE,
    // with the `seq_layout` block above, and they read slot 2 and the byteSize
    // ratio. A second pair reading slot 4 and slot 5 stood here — the accessors
    // for the six-slot sequence carrier that F16 replaced. Registration is
    // last-write-wins, so the later pair silently took the rows away from the
    // encoding the rest of this file walks: on a four-slot carrier slot 4 and
    // slot 5 are past the end of the object, and `elementLayout()` would have
    // answered nothing for every sequence. One encoding, one pair of readers.

    // Linker
    let gl = "java/lang/foreign/GroupLayout";
    for layout_class in [
        gl,
        "java/lang/foreign/StructLayout",
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "jdk/internal/foreign/layout/UnionLayoutImpl",
    ] {
        r.register(
            layout_class,
            "memberLayouts",
            "()Ljava/util/List;",
            |ctx, args| {
                let members =
                    match obj_arg(args, 0).ok().and_then(|this| {
                        match ctx.get_field(this, p67_layout_slots(ctx, this).payload) {
                            Value::Object(Some(arr)) => Some(arr),
                            _ => None,
                        }
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
                let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", n_fields)?;
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

    // `UnionLayout` and `PaddingLayout` had NO size/alignment accessors at all
    // (F16, 2026-08-13). The group loop above covers `GroupLayout` and
    // `StructLayout` only, so `unionLayout(...).byteSize()` and
    // `paddingLayout(3).byteSize()` resolved to the abstract interface
    // declaration — `AbstractMethodError: ... has no Code attribute` — which is
    // how the union stub's fabricated `Long(0)` stayed invisible: nothing could
    // read it. Both carriers now have the shared four-slot head, so both share
    // the same two readers.
    //
    // Plain `java/lang/foreign/ValueLayout` is here for the same reason: the
    // `$Of*` loop above registers the accessors on each concrete spelling but
    // never on the interface itself, and `panama.rs` used to supply that row
    // from the OTHER encoding (reading slot 1 as `Int`). With panama's minter
    // gone the row has to exist here, in the encoding that is now the only one.
    for layout_class in [
        "java/lang/foreign/UnionLayout",
        "java/lang/foreign/PaddingLayout",
        "java/lang/foreign/ValueLayout",
    ] {
        r.register(layout_class, "byteSize", "()J", p67_layout_byte_size);
        r.register(
            layout_class,
            "byteAlignment",
            "()J",
            p67_layout_byte_alignment,
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
    // differs from the native MemorySegment bridge. On the impl class as well,
    // for the receiver-class reason above -- `allocate(JJ)` is already on both.
    r.register(
        "java/lang/foreign/Arena",
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        crate::panama::pe_arena_allocate,
    );
    r.register(
        "jdk/internal/foreign/ArenaImpl",
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        crate::panama::pe_arena_allocate,
    );
    // Both names, for the reason `register_p67_segment_surface` above carries:
    // native dispatch is keyed on the receiver's class, and a CratonVM-minted
    // segment is stamped with `panama::CRATON_SEGMENT_CLASS`, not the interface.
    for ms in [
        crate::panama::PE_SEGMENT_INTERFACE,
        crate::panama::CRATON_SEGMENT_CLASS,
    ] {
        // Was an inline closure that copied slots 0..5 across verbatim and had
        // NO native-access check. Both halves were wrong on a shipping binary:
        // the raw slot copy assumes the synthetic six-slot layout on a receiver
        // that need not have it, and `reinterpret` is exactly the call real JDK
        // 25 restricts -- it hands back an arbitrary-size window over a
        // possibly-raw address. The synthetic-only twin had both right and never
        // shipped; it is now the only body, and lives in `panama.rs`.
        r.register(
            ms,
            "reinterpret",
            "(J)Ljava/lang/foreign/MemorySegment;",
            crate::panama::pe_segment_reinterpret,
        );
        // The JDK-21-preview spelling of `getString`, and now the same body.
        // It was an independent closure reading `get_field(this, 0)` as the base
        // address -- correct ONLY for the synthetic six-slot carrier. On a real
        // JDK-loaded segment slot 0 is the segment's byte LENGTH, which is the
        // precise confusion `panama_libffi::segment_address` exists to end; its
        // own comment records `ofArray(new byte[16])` faulting at `address 0x10`,
        // and 0x10 == 16 == that array's length. `p67_segment_get_string` reads
        // through `segment_address`/`segment_byte_size`, which accept BOTH
        // segment models, and raises `IndexOutOfBoundsException` where the JDK
        // does rather than `IllegalStateException`.
        r.register(
            ms,
            "getUtf8String",
            "(J)Ljava/lang/String;",
            p67_segment_get_string,
        );
    }

    r.register(
        linker,
        "nativeLinker",
        "()Ljava/lang/foreign/Linker;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        linker,
        "defaultLookup",
        "()Ljava/lang/foreign/SymbolLookup;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 2)?;
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

            if crate::nbflags().dbg_linker {
                eprintln!(
                    "[LATE_LINKER] option downcall addr=0x{fn_addr:x} options={}",
                    args.get(3).is_some()
                );
            }
            // P1-E: this WAS the live `--jdk-only` mint of
            // `java/lang/foreign/DowncallHandle`, a class no real JDK image
            // declares, so strict mode correctly refused it and every FFM
            // downcall died as
            // `NoClassDefFoundError: java/lang/foreign/DowncallHandle` at the
            // application's call site. The refusal was right; the survival of
            // this caller was the defect.
            //
            // The carrier is now a real `java/lang/invoke/MethodHandle` — which
            // is what the caller actually holds it as, casts it to, and calls
            // `invokeExact` on. See `panama::alloc_downcall_handle`.
            //
            // Note the panama.rs mints of the same class are NOT this one:
            // `register_pe_panama` is reached only from
            // `register_synthetic_overrides`, which is
            // `#[cfg(feature = "synthetic-jdk")]`, so it is in neither shipping
            // binary. This site, reached from
            // `register_essential_natives_with_shims`, is the one that ran.
            let dh = crate::panama::alloc_downcall_handle(
                ctx,
                fn_addr,
                descriptor,
                variadic_fixed,
                capture_call_state,
            )?;
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
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
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
            let obj =
                try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
            ctx.set_field(obj, 0, Value::Object(None));
            ctx.set_field(obj, 1, Value::Object(Some(params)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // G19-1: THE TWO READERS OF THE CARRIER THE TWO FACTORIES ABOVE MINT.
    //
    // MEASURED, `--jdk-only` against 25.0.3+9-LTS, before this registration:
    //
    //     FunctionDescriptor.of(JAVA_LONG, ADDRESS).returnLayout()
    //       -> AbstractMethodError: method java/lang/foreign/FunctionDescriptor
    //          .returnLayout()Ljava/util/Optional; has no Code attribute
    //     ... .argumentLayouts()
    //       -> AbstractMethodError: ... .argumentLayouts()Ljava/util/List;
    //
    // That is the whole of `RJdkForeign`'s `[layouts]` step failure: the two
    // factories are registered HERE — `register_pe_function_descriptor` in
    // `panama.rs`, which does carry a `returnLayout`, is reached only from
    // `register_pe_panama`/`register_synthetic_overrides` and does not run in
    // `--jdk-only` (registry dump: the only two `FunctionDescriptor` rows are
    // `of` and `ofVoid`, both `foreign_ffm.rs`, `owns_slot=true`). So the
    // carrier was mintable and unreadable.
    //
    // Both are declared ABSTRACT on the sealed interface and have no `Object`
    // fallback, which is why a registration here wins where one for
    // `toString`/`equals` would not — see the record's NOM-2.
    //
    // `argumentLayouts()` is `java.util.List`, NOT `ValueLayout[]`. The array
    // spelling is the pre-JDK-22 preview signature and it is what
    // `panama.rs`'s dead copy still registers; a caller writing `.size()` on
    // the JDK 22+ API would have got the same `AbstractMethodError` even after
    // that registrar was reached.
    r.register(fd, "returnLayout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // MEASURED: `of(JAVA_LONG, ADDRESS).returnLayout()` is `Optional[j8]`
        // and `ofVoid(JAVA_INT).returnLayout()` is `Optional.empty` — so the
        // void carrier's null slot 0 must become an EMPTY Optional and not a
        // present one holding null. `p67_optional` is the local helper the
        // `name()`/`targetLayout()` readers already use, and it is measured
        // working on a real `java.util.Optional` in `--jdk-only`
        // (`JAVA_LONG.name()` prints `Optional.empty` today).
        let value = ctx.get_field(this, 0);
        let opt = p67_optional(ctx, value)?;
        Ok(Some(Value::Object(Some(opt))))
    });
    r.register(fd, "argumentLayouts", "()Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Read the slot into a local FIRST: a `match` whose scrutinee is a
        // `&self` call keeps that borrow alive for the whole match, and the
        // fallback arm needs `&mut ctx` to mint the empty array.
        let stored = ctx.get_field(this, 1);
        let array = match stored {
            Value::Object(Some(arr)) => arr,
            _ => ctx.new_array(ArrayElementType::Reference, 0),
        };
        // `List.of` with an `Arrays.asList` fallback is this tree's idiom for
        // handing back an immutable list (`lang_invoke::vh_coordinate_types`),
        // and it is measured working in this binary: `RJdkForeign`'s
        // `layoutVarHandles` step asserts
        // `coordinateTypes().equals(List.of(MemorySegment.class, long.class))`
        // and is green. The fallback matters for a descriptor built with a
        // null member, which `List.of` refuses and `Arrays.asList` accepts —
        // the oracle refuses that descriptor at the FACTORY (NPE), which this
        // lane did not change, so the fallback keeps a carrier that already
        // exists readable rather than turning a read into a second refusal.
        let array_pin = ctx.pin_native_root(array);
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
        ctx.unpin_native_roots(array_pin);
        Ok(Some(list.unwrap_or(Value::Object(None))))
    });

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

#[cfg(test)]
mod g19_scope_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    /// Build the synthetic segment shape `panama::pe_of_array_alias` and
    /// `panama::pe_segment_slice` mint: eight slots, `[1]=byteSize`, and
    /// `[2]` = the segment's own session.
    fn stamped_segment(ctx: &mut dyn NativeContext, session: Value, byte_size: i64) -> ObjectRef {
        let seg = crate::panama::alloc_segment_carrier(ctx, 8).unwrap();
        ctx.set_field(seg, 0, Value::Long(0));
        ctx.set_field(seg, 1, Value::Long(byte_size));
        ctx.set_field(seg, P67_SEGMENT_ARENA, session);
        ctx.set_field(seg, 3, Value::Int(0));
        ctx.set_field(seg, 4, Value::Int(1));
        ctx.set_field(seg, 5, Value::Long(0));
        seg
    }

    /// G19-1: `scope()` on a carrier stamped with its session must hand back
    /// THAT session, not a fresh one.
    ///
    /// MEASURED on 25.0.3+9-LTS: `heap.scope() == heap.scope()` and
    /// `seg.asSlice(4,4).scope() == seg.scope()` are both true; CratonVM
    /// answered false for both because this reader only ever looked for an
    /// ARENA in slot 2, while `panama::pe_segment_slice` had been stamping the
    /// SESSION there since W7-89. The `RForeignLayoutJdkInterfaces` assertion
    /// "a heap segment's scope is stable" is this row.
    #[test]
    fn a_stamped_session_is_the_scope_and_it_is_the_same_object_every_time() {
        let mut ctx = mock_ctx();
        let session = p67_memory_session(&mut ctx).unwrap();
        let seg = stamped_segment(&mut ctx, session, 16);

        let first = p67_receiver_session(&mut ctx, seg).unwrap();
        let second = p67_receiver_session(&mut ctx, seg).unwrap();
        assert_eq!(first, session, "scope() must answer the stamped session");
        assert_eq!(
            first, second,
            "scope() must answer the SAME object on every call"
        );
    }

    /// The negative half, and it is not optional: slot 2's other tenant on an
    /// `ofArray` MIRROR carrier is the Java backing array, and a reader that
    /// accepted it as a session would read an array element as a state word.
    ///
    /// A carrier with nothing in slot 2 keeps the historical behaviour (a fresh
    /// session), which is a separate, still-open divergence — see the record's
    /// §"what this lane did NOT do".
    #[test]
    fn an_array_in_slot_two_is_not_a_scope() {
        let mut ctx = mock_ctx();
        let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        let seg = stamped_segment(&mut ctx, Value::Object(Some(array)), 16);
        let scope = p67_receiver_session(&mut ctx, seg).unwrap();
        assert_ne!(
            scope,
            Value::Object(Some(array)),
            "the backing array must never be handed out as a scope"
        );
        assert_eq!(
            ctx.class_name_of_id(ctx.class_id_of_object(match scope {
                Value::Object(Some(obj)) => obj,
                other => panic!("scope() answered {other:?}"),
            }))
            .as_deref(),
            Some("jdk/internal/foreign/MemorySessionImpl"),
            "the fallback is still a session"
        );
    }

    /// G19-1: and the stamped session is CHECKED, not merely reported.
    ///
    /// The arm added to `p67_receiver_session` without the matching arm here
    /// would be the W7-89 fail-open one branch over: the one shape whose scope
    /// resolves would be the one shape that skips the validity check. Oracle:
    /// a closed arena's segment answers `IllegalStateException: Already closed`
    /// on every access.
    #[test]
    fn a_stamped_session_that_has_closed_refuses_the_access() {
        let mut ctx = mock_ctx();
        let session = p67_memory_session(&mut ctx).unwrap();
        let mut seg = stamped_segment(&mut ctx, session, 16);
        assert!(
            p67_segment_check_scope(&mut ctx, &mut seg).is_ok(),
            "an open session must let the access through"
        );

        let session_obj = match session {
            Value::Object(Some(obj)) => obj,
            other => panic!("p67_memory_session answered {other:?}"),
        };
        let slots = p67_session_slots(&ctx, session_obj);
        ctx.set_field(session_obj, slots.state, Value::Int(0));
        let err = p67_segment_check_scope(&mut ctx, &mut seg).unwrap_err();
        assert!(
            format!("{err:?}").contains("Already closed"),
            "a closed stamped session must refuse; got {err:?}"
        );
    }
}

/// The `java.lang.foreign.MemorySegment` natives phase 67 owns, registered
/// under one receiver class.
///
/// Extracted from `register_p67_foreign_memory`'s body (2026-08-22) for the
/// same reason `panama::register_pe_memory_segment_on` was: the set has to be
/// registered on TWO names now — the interface, for a call site that resolved
/// against the abstract declaration, and `panama::CRATON_SEGMENT_CLASS`, which
/// is the runtime class of every segment CratonVM mints. A loop over the two
/// cannot drift; two hand-maintained copies can.
fn register_p67_segment_surface(r: &mut NativeMethodRegistry, ms: &str) {
    // MemorySegment = 2-field (byteSize=0 Long, address=1 Long)
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
                let seg = crate::panama::alloc_segment_carrier(ctx, 6)?;
                ctx.set_field(seg, 0, Value::Long(base_ptr));
                ctx.set_field(seg, 1, Value::Long(size));
                ctx.set_field(seg, 2, ctx.get_field(this, 2));
                ctx.set_field(seg, 3, ctx.get_field(this, 3));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(base_off + offset));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                let seg = crate::panama::alloc_segment_carrier(ctx, 2)?;
                ctx.set_field(seg, 0, Value::Long(size));
                ctx.set_field(seg, 1, Value::Long(offset));
                Ok(Some(Value::Object(Some(seg))))
            }
        },
    );
    // SHADOWED: `panama.rs::register_pe_memory_segment` registers this exact
    // class+method+descriptor too, and its registrar runs AFTER this one on
    // both paths that reach them (`register_essential_natives`: foreign_ffm at
    // lib.rs:9736 then panama at :9740; full registry: phase 67 at :22995 then
    // `register_pe_panama` at :23051). Last-write-wins, so panama's is the live
    // answer and this one is dead — yet it used to say FALSE where panama said
    // TRUE, a contradiction that would have bitten whoever changed the
    // registration order. Panama now answers from the receiver (heap-backed
    // `ofArray` segments are not native, everything else is); the arena-backed
    // carriers this file mints are all off-heap, so TRUE is this fallback's
    // correct value and the two no longer disagree.
    //
    // VERIFIED (wave 4): `panama.rs:720` reads `SEG_BACKING_ARRAY_FIELD` off the
    // receiver and answers `!heap_backed`; both call sites still order
    // foreign_ffm before panama, so panama's remains live and the two agree.
    // Left as a constant deliberately — routing it through panama's
    // receiver-based check would answer FALSE for the synthetic carriers here,
    // which have no backing-array field at all, i.e. it would introduce the
    // contradiction this note exists to record.
    r.register(ms, "isNative", "()Z", |_ctx, _args| Ok(Some(Value::Int(1))));
    // STUB-REMOVAL (wave 4): was a flat `false`. The answer is decidable from
    // the receiver by exactly the same rule the three impl classes use below
    // (`MappedMemorySegmentImpl` is mapped, nothing else is), so share the
    // helper rather than restate a constant that could drift away from it.
    //
    // The value does not change today: `MemorySegment` is an INTERFACE and this
    // is a non-static instance method, so per the interface-shadowing rule this
    // registration only ever reaches the synthetic `java/lang/foreign
    // /MemorySegment` carriers this file mints — every one of which comes from
    // `Arena.allocate*`/`asSlice`/`reinterpret`/`NULL`/`ofArray` and is
    // malloc- or heap-backed, never `FileChannel.map`. It stops being a
    // constant the moment a mapped carrier is minted.
    r.register(ms, "isMapped", "()Z", p67_segment_impl_is_mapped);
    r.register(ms, "isReadOnly", "()Z", p67_segment_is_read_only);
    // The JDK 22+ spelling of the C-string read. See `p67_segment_get_string`
    // for why nothing answered it before. The `(long, Charset)` overload is
    // deliberately NOT registered: this implementation decodes UTF-8, and
    // answering a caller that asked for another charset with UTF-8 bytes would
    // be a wrong value where the AbstractMethodError is at least a refusal.
    r.register(
        ms,
        "getString",
        "(J)Ljava/lang/String;",
        p67_segment_get_string,
    );
    // `AbstractMemorySegmentImpl` DOES override `equals`, and the comment that
    // used to sit here — "segment equality IS reference identity" — was written
    // from memory. MEASURED against HotSpot 25.0.4+7
    // (`apps/probes/FfmMsgProbe.java`):
    //
    //   MemorySegment s = arena.allocate(16);
    //   s.asSlice(0, 16).equals(s)   true      <- two DIFFERENT objects
    //   s.asSlice(0, 16) == s        false
    //   s.asSlice(0, 16).hashCode() == s.hashCode()   true
    //
    // so it is VALUE equality over the pair the JDK hashes: the base and the
    // offset. Length is deliberately NOT part of it — `equals` is asking "do
    // these two references point at the same place", which is the question the
    // aliasing guards FFM callers write are actually asking.
    //
    // This is the second correction to this one line. The constant `false` it
    // started as broke reflexivity; identity fixed that and stopped one row
    // short. A segment and a full-length slice of it are the same place, and a
    // caller that de-duplicates segments before a bulk copy saw two.
    //
    // `panama_libffi::segment_address` is the shared reader for "where does
    // this carrier point", already `[0] + [5]` and already 0 for a heap
    // segment (F27) — so heap segments fall back to comparing their BACKING
    // ARRAY, which is the `unsafeGetBase()` half of the JDK's pair. Without
    // that, every heap segment would equal every other one at address 0.
    r.register(ms, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let Some(Value::Object(Some(other))) = args.get(1).copied() else {
            return Ok(Some(Value::Int(0)));
        };
        if other == this {
            return Ok(Some(Value::Int(1)));
        }
        let same_address = crate::panama_libffi::segment_address(&*ctx, this)
            == crate::panama_libffi::segment_address(&*ctx, other);
        // The base half. `heapBase()` is registered on both spellings and
        // answers an `Optional`, so the raw slot is read instead: two heap
        // segments over the same array are the same place, two over different
        // arrays are not, and a native segment has no base on either side.
        let this_base = p67_segment_heap_base_slot(ctx, this);
        let other_base = p67_segment_heap_base_slot(ctx, other);
        let same_base = match (this_base, other_base) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        Ok(Some(Value::Int(i32::from(same_address && same_base))))
    });
    // `hashCode` OVER THE SAME PAIR `equals` USES. Registering one without the
    // other is the classic split: `s.asSlice(0, s.byteSize()).equals(s)` is
    // true and the two hashed differently, so a `HashSet<MemorySegment>` held
    // the same place twice and a de-duplicating caller never saw it.
    //
    // The answer was already recorded in `equals`'s own comment above --
    // `s.asSlice(0, 16).hashCode() == s.hashCode()` is `true` on HotSpot --
    // three lines from the registration that was never written. Without this,
    // real `AbstractMemorySegmentImpl.hashCode` runs and reads
    // `length`/`readOnly`/`scope`, which this carrier does not have.
    //
    // The VALUE is not compared against HotSpot's and must not be: the JDK
    // hashes `unsafeGetBase()` and `unsafeGetOffset()`, and an identity hash of
    // a base array is not reproducible across VMs. What IS comparable, and what
    // the contract requires, is that equal segments hash equal -- MEASURED by
    // `apps/probes/FfmMsgProbe.java`.
    r.register(ms, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let address = crate::panama_libffi::segment_address(&*ctx, this);
        let base = p67_segment_heap_base_slot(ctx, this)
            .map(|b| ctx.identity_hash_code(b))
            .unwrap_or(0);
        // `Objects.hash(base, offset)`'s shape: the JDK's own combiner, so two
        // segments differing in either half are unlikely to collide.
        let h = 31i32
            .wrapping_mul(31i32.wrapping_add(base))
            .wrapping_add(address as i32)
            ^ ((address >> 32) as i32);
        Ok(Some(Value::Int(h)))
    });
    r.register(
        ms,
        "scope",
        "()Ljava/lang/foreign/MemorySegment$Scope;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(p67_receiver_session(ctx, this)?))
        },
    );
    r.register(
        ms,
        "NULL",
        "Ljava/lang/foreign/MemorySegment;",
        |ctx, _args| {
            let seg = crate::panama::alloc_segment_carrier(ctx, 2)?;
            ctx.set_field(seg, 0, Value::Long(0));
            ctx.set_field(seg, 1, Value::Long(0));
            Ok(Some(Value::Object(Some(seg))))
        },
    );
}
