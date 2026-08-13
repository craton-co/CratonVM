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
    let obj = try_alloc_concurrent_synthetic(ctx, class_name, 4)?;
    ctx.set_field(obj, 0, Value::Long(byte_size));
    ctx.set_field(obj, 1, Value::Long(byte_alignment));
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
    ctx.set_field(
        obj,
        2,
        Value::Int(i32::from(cfg!(target_endian = "little"))),
    );
    ctx.set_field(obj, 3, Value::Object(None));
    Ok(obj)
}

pub(crate) fn p67_optional(ctx: &mut dyn NativeContext, value: Value) -> Result<ObjectRef, MethodCallFailed> {
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

pub(crate) fn p67_class_mirror(ctx: &mut dyn NativeContext, class_name: &str) -> Result<ObjectRef, MethodCallFailed> {
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
    let size = match ctx.get_field(layout, 0) {
        Value::Long(v) => v,
        Value::Int(v) => i64::from(v),
        _ => 0,
    };
    let align = match ctx.get_field(layout, 1) {
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
pub(crate) fn p67_byte_order_object(ctx: &mut dyn NativeContext, little_endian: bool) -> Result<ObjectRef, MethodCallFailed> {
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
    let obj = p67_layout_object(ctx, &class_name, byte_size, byte_alignment)?;
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
const P67_ARENA_SLOTS: usize = 2;

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
fn p67_receiver_session(ctx: &mut dyn NativeContext, receiver: ObjectRef) -> Result<Value, MethodCallFailed> {
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
        if let Value::Object(Some(arena)) = ctx.get_field(receiver, P67_SEGMENT_ARENA) {
            if let Some(session) = p67_arena_session(ctx, arena) {
                return Ok(Value::Object(Some(session)));
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
    if ctx.object_num_fields(arena) <= P67_ARENA_SESSION {
        return None;
    }
    match ctx.get_field(arena, P67_ARENA_SESSION) {
        Value::Object(Some(session)) if p67_session_modelled(ctx, session) => Some(session),
        _ => None,
    }
}

/// Allocate a synthetic Arena together with the session that gives it a
/// lifetime. `confined` records the calling thread as the session owner, which
/// is what lets an off-thread access raise `WrongThreadException`; a shared or
/// automatic arena leaves the owner null.
fn p67_new_arena(ctx: &mut dyn NativeContext, confined: bool) -> Result<ObjectRef, MethodCallFailed> {
    let arena = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", P67_ARENA_SLOTS)?;
    // The session allocation below can move the fresh arena (native stale-local
    // family).
    let arena_pin = ctx.pin_native_root(arena);
    let session_value = p67_memory_session(ctx)?;
    let arena = ctx.read_native_pin(arena_pin, arena);
    ctx.unpin_native_roots(arena_pin);
    ctx.set_field(arena, P67_ARENA_OPEN, Value::Int(1));
    ctx.set_field(arena, P67_ARENA_SESSION, session_value);
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
fn p67_arena_segment(ctx: &mut dyn NativeContext, arena: ObjectRef, size: i64) -> Result<ObjectRef, MethodCallFailed> {
    let arena_pin = ctx.pin_native_root(arena);
    let segment = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 3)?;
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
fn p67_segment_check_scope(
    ctx: &mut dyn NativeContext,
    segment: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if let Value::Object(Some(scope)) = ctx.get_field_by_name(segment, "scope") {
        if p67_session_is_real(ctx, scope) {
            ctx.invoke_virtual_bytecode_only(scope, "checkValidState", "()V", &[])?;
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
            p67_session_check_valid(ctx, scope)?;
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
    if ctx.object_num_fields(segment) > P67_SEGMENT_ARENA {
        if let Value::Object(Some(arena)) = ctx.get_field(segment, P67_SEGMENT_ARENA) {
            if let Some(session) = p67_arena_session(ctx, arena) {
                p67_session_check_valid(ctx, session)?;
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
    session: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if !p67_session_modelled(ctx, session) {
        return Ok(());
    }
    let slots = p67_session_slots(ctx, session);
    let owner = match ctx.get_field(session, slots.owner) {
        Value::Object(Some(owner)) => Some(owner),
        _ => None,
    };
    if let Some(owner) = owner {
        if ctx.current_thread_object() != owner {
            return Err(p67_wrong_thread(ctx));
        }
    }
    if p67_session_state(ctx, session) == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Already closed".to_string(),
        }
        .into());
    }
    Ok(())
}

fn p67_session_acquire(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
) -> Result<(), MethodCallFailed> {
    p67_session_check_valid(ctx, session)?;
    if p67_session_modelled(ctx, session) {
        let count = p67_session_acquires(ctx, session);
        let slots = p67_session_slots(ctx, session);
        ctx.set_field(session, slots.acquires, Value::Int(count.saturating_add(1)));
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
    session: ObjectRef,
) -> Result<(), MethodCallFailed> {
    p67_session_check_valid(ctx, session)?;
    if !p67_session_modelled(ctx, session) {
        return Ok(());
    }
    let acquired = p67_session_acquires(ctx, session);
    if acquired > 0 {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Session is acquired by {acquired} clients"),
        }
        .into());
    }
    let slots = p67_session_slots(ctx, session);
    ctx.set_field(session, slots.state, Value::Int(0));
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
    let this = obj_arg(args, 0)?;
    p67_session_check_valid(ctx, this)?;
    let Some(Value::Object(Some(action))) = args.get(1) else {
        return Ok(None);
    };
    if p67_session_modelled(ctx, this) {
        p67_session_push_action(ctx, this, *action);
    }
    Ok(None)
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

pub(crate) fn p67_var_handle_for_layout(ctx: &mut dyn NativeContext, layout: ObjectRef) -> Result<Value, MethodCallFailed> {
    let width = p67_layout_width_obj(ctx, layout);
    let little_endian = p67_layout_is_little(ctx, layout);
    let vh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS)?;
    ctx.set_field(
        vh,
        VH_CLASS_OR_TARGET,
        Value::Int(if little_endian { 1 } else { 0 }),
    );
    ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(width));
    ctx.set_field(vh, VH_IS_STATIC, Value::Int(VH_KIND_MEMORY_SEGMENT));
    crate::lang_invoke::register_p67_memory_segment_var_handle(ctx, vh, width);
    Ok(Value::Object(Some(vh)))
}

pub(crate) fn p67_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> Result<Value, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(layout))) => p67_var_handle_for_layout(ctx, *layout),
        _ => {
            let vh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_NUM_FIELDS)?;
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
    let target_layout = match args.get(1) {
        Some(Value::Object(Some(path_arr))) => p67_memory_layout_path_target(ctx, this, *path_arr),
        _ => this,
    };
    Ok(Some(p67_var_handle_for_layout(ctx, target_layout)?))
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
    p67_segment_check_scope(ctx, seg)?;
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
    p67_segment_check_scope(ctx, seg)?;
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
    // (fixed-suite-bugs/s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md
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
    r.register(
        arena,
        "ofAuto",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| Ok(Some(Value::Object(Some(p67_new_arena(ctx, false)?)))),
    );
    r.register(
        arena,
        "ofShared",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| Ok(Some(Value::Object(Some(p67_new_arena(ctx, false)?)))),
    );
    r.register(
        arena,
        "global",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _args| Ok(Some(Value::Object(Some(p67_new_arena(ctx, false)?)))),
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
    r.register(arena, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Close the arena's session FIRST: a second `close()` must surface the
        // session's IllegalStateException rather than silently re-clearing the
        // flag, and the cleanups have to run while the arena is still open.
        if let Some(session) = p67_arena_session(ctx, this) {
            p67_session_just_close(ctx, session)?;
            p67_session_run_close_actions(ctx, session)?;
        }
        ctx.set_field(this, P67_ARENA_OPEN, Value::Int(0));
        Ok(None)
    });
    r.register(
        arena,
        "scope",
        "()Ljava/lang/foreign/MemorySegment$Scope;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(p67_receiver_session(ctx, this)?))
        },
    );
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
        let this = obj_arg(args, 0)?;
        p67_session_acquire(ctx, this)?;
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
            let this = obj_arg(args, 0)?;
            let action = obj_arg(args, 1)?;
            // The acquire count is what keeps the session alive across the
            // callback: a nested `close()` sees a non-zero count and refuses,
            // exactly as the JDK's `whileAlive` does. Released even when the
            // action throws, or the session could never be closed afterwards.
            p67_session_acquire(ctx, this)?;
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
        let this = obj_arg(args, 0)?;
        p67_session_check_valid(ctx, this)?;
        Ok(None)
    });
    r.register(session, "checkValidState", "()V", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "checkValidState", "()V") {
            return real;
        }
        let this = obj_arg(args, 0)?;
        p67_session_check_valid(ctx, this)?;
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
            let segment = obj_arg(args, 0)?;
            p67_segment_check_scope(ctx, segment)?;
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
        let this = obj_arg(args, 0)?;
        p67_session_just_close(ctx, this)?;
        p67_session_run_close_actions(ctx, this)?;
        Ok(None)
    });
    r.register(session, "justClose", "()V", |ctx, args| {
        if let Some(real) = p67_session_delegate(ctx, args, "justClose", "()V") {
            return real;
        }
        let this = obj_arg(args, 0)?;
        p67_session_just_close(ctx, this)?;
        Ok(None)
    });

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
                let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
                ctx.set_field(seg, 0, Value::Long(base_ptr));
                ctx.set_field(seg, 1, Value::Long(size));
                ctx.set_field(seg, 2, ctx.get_field(this, 2));
                ctx.set_field(seg, 3, ctx.get_field(this, 3));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(base_off + offset));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 2)?;
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
    // `MemorySegment` does not override `equals` in the JDK — segment equality
    // IS reference identity. The constant `false` this used to return broke
    // even reflexivity (`seg.equals(seg)` was false), so a segment could not be
    // found in any collection it had just been put into, and the
    // `slice.equals(other)` guards FFM callers write around aliasing all took
    // the wrong branch.
    r.register(ms, "equals", "(Ljava/lang/Object;)Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let equal = matches!(args.get(1), Some(Value::Object(Some(other))) if *other == this);
        Ok(Some(Value::Int(i32::from(equal))))
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
            let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 2)?;
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
        // `isNative`/`isMapped` were a constant TRUE for all three classes,
        // which is right for only two of the six answers — a NATIVE segment is
        // not mapped, and `AbstractMemorySegmentImpl` is the shared base of the
        // HEAP impls too, so a blanket TRUE there reported every heap segment
        // as both native and mapped. Answer from the receiver's own runtime
        // class instead (see `p67_segment_impl_is_native`).
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
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/StructLayout", 4)?;
            let members = ctx.read_native_pin(members_pin, members);
            ctx.set_field(obj, 0, Value::Long(total_size));
            ctx.set_field(obj, 1, Value::Long(max_align));
            ctx.set_field(obj, 2, Value::Object(Some(members)));
            ctx.set_field(obj, 3, Value::Object(None));
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
            let count = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let element = match args.get(1) {
                Some(Value::Object(Some(obj))) => Some(*obj),
                _ => None,
            };
            let (elem_size, elem_align) = element.map_or((0, 1), |e| p67_layout_size_align(ctx, e));
            let element_pin = element.map(|e| (ctx.pin_native_root(e), e));
            // Six slots: the standard four plus the element layout and count,
            // so `elementLayout()`/`elementCount()` have somewhere to read from
            // when they are implemented.
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/SequenceLayout", 6)?;
            let element = element_pin.map(|(pin, e)| {
                let e = ctx.read_native_pin(pin, e);
                ctx.unpin_native_roots(pin);
                e
            });
            ctx.set_field(obj, 0, Value::Long(count.max(0).saturating_mul(elem_size)));
            ctx.set_field(obj, 1, Value::Long(elem_align));
            ctx.set_field(
                obj,
                2,
                Value::Int(i32::from(cfg!(target_endian = "little"))),
            );
            ctx.set_field(obj, 3, Value::Object(None));
            ctx.set_field(obj, 4, Value::Object(element));
            ctx.set_field(obj, 5, Value::Long(count));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ml,
        "unionLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;",
        |ctx, args| {
            // A union is as large as its largest member, rounded up to the
            // strictest member alignment — not zero, which is what this
            // returned for every union regardless of its members.
            let members = match args.first() {
                Some(Value::Object(Some(arr))) => Some(*arr),
                _ => None,
            };
            let (mut size, mut align) = (0_i64, 1_i64);
            if let Some(members) = members {
                for i in 0..ctx.array_length(members) {
                    if let Value::Object(Some(member)) = ctx.get_array_element(members, i) {
                        let (m_size, m_align) = p67_layout_size_align(ctx, member);
                        size = size.max(m_size);
                        align = align.max(m_align);
                    }
                }
            }
            let size = ((size + align - 1) / align).saturating_mul(align);
            let obj = p67_layout_object(ctx, "java/lang/foreign/UnionLayout", size, align)?;
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
            // Padding has no alignment constraint of its own — the JDK's
            // `PaddingLayoutImpl` is byte-aligned.
            let obj = p67_layout_object(ctx, "java/lang/foreign/PaddingLayout", size, 1)?;
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
    r.register(ml, "byteSize", "()J", p67_layout_byte_size);
    r.register(ml, "byteAlignment", "()J", p67_layout_byte_alignment);
    for layout_class in [
        "java/lang/foreign/SequenceLayout",
        "java/lang/foreign/PaddingLayout",
        "java/lang/foreign/UnionLayout",
    ] {
        r.register(layout_class, "byteSize", "()J", p67_layout_byte_size);
        r.register(
            layout_class,
            "byteAlignment",
            "()J",
            p67_layout_byte_alignment,
        );
    }

    // `SequenceLayout`'s own two accessors. The factory above already keeps the
    // element layout at slot 4 and the count at slot 5 "so `elementLayout()`/
    // `elementCount()` have somewhere to read from when they are implemented" —
    // this is that.
    r.register(
        "java/lang/foreign/SequenceLayout",
        "elementLayout",
        "()Ljava/lang/foreign/MemoryLayout;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 4)))
        },
    );
    r.register(
        "java/lang/foreign/SequenceLayout",
        "elementCount",
        "()J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(match ctx.get_field(this, 5) {
                v @ Value::Long(_) => v,
                _ => Value::Long(0),
            }))
        },
    );

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
            let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
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
            let dh = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/DowncallHandle", 5)?;
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
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
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
    ()
}
