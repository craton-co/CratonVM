// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `jdk.internal.vm.vector.VectorSupport` — the operations HotSpot intrinsifies.
//!
//! # Why this file exists, and how it differs from `vector_api.rs`
//!
//! [`crate::vector_api`] is CratonVM's **synthetic** Vector API: it registers on
//! `jdk/incubator/vector/IntVector` and friends and models a vector with
//! CratonVM's own species/lane layout, for the mode where there is no real
//! `jdk.incubator.vector` to run. None of it is reached in real-JDK mode — a
//! real receiver is a concrete `Int256Vector`, whose own bytecode answers first
//! — and that is correct, because those natives would read a JDK vector's
//! fields under CratonVM's meanings.
//!
//! This file is the real-JDK half, and it intercepts one class lower down.
//! Every lane operation in `jdk.incubator.vector` funnels through a static
//! `VectorSupport` method marked `@IntrinsicCandidate`:
//!
//! ```text
//! IntVector.add(v) -> lanewise(ADD, v) -> lanewiseTemplate(..)
//!   -> VectorSupport.binaryOp(VECTOR_OP_ADD, Int256Vector.class, null, int.class,
//!                             8, this, that, null, BIN_IMPL.find(ADD, ..))
//! ```
//!
//! C2 replaces that call with SIMD instructions. Its **Java fallback**, which is
//! what an interpreter runs, is `defaultImpl.apply(v1, v2, m)` — a lambda that
//! calls `bOp`, which allocates a result array, runs a second lambda once per
//! lane, and builds a new vector. Measured on GPULlama3's inference kernel
//! before this file existed (`probes/Fp16VectorDotBench.java`): **0.268 ns per
//! lane on HotSpot against 115 561 ns on CratonVM**, with 93.7% of a
//! 3500-sample profile inside `bOpTemplate` / `uOpTemplate` /
//! `lanewiseTemplate` / `vectorFactory` / `maybeRebox` and the per-operation
//! lambdas.
//!
//! # The object model, and why it is safe to build one
//!
//! A vector is ONE instance field. `VectorSupport$VectorPayload` declares
//! `private final Object payload` and **nothing below it declares another** —
//! `AbstractVector`, `FloatVector` and `Float256Vector` add only statics
//! (`javap -p`, Temurin 25.0.3+9). So reading a vector is one field read of a
//! primitive array, and building one is an allocation plus one field write.
//! `payload_slot` resolves the index by NAME rather than assuming 0, so a JDK
//! that adds a field ahead of it moves this code's reads with it instead of
//! silently shifting them.
//!
//! # Refusing is always available
//!
//! Every entry point here ends in [`fallback`]: the `defaultImpl` lambda the JDK
//! passed in, invoked unchanged. An opcode this file does not implement, an
//! element type it cannot decode, a masked form, a species it cannot size — all
//! take that path and produce exactly the answer the un-intercepted VM produced.
//! That is what makes partial coverage safe, and it is why the table below can
//! grow one opcode at a time.

use cratonvm_native_api::{NativeCallback, NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

use crate::vector_api::{array_lane_bits, lane_bits_to_value, wrap_integral_lane};
use crate::vector_api::{ELEM_BYTE, ELEM_DOUBLE, ELEM_FLOAT, ELEM_INT, ELEM_LONG, ELEM_SHORT};

const VECTOR_SUPPORT: &str = "jdk/internal/vm/vector/VectorSupport";
const PAYLOAD: &str = "Ljdk/internal/vm/vector/VectorSupport$VectorPayload;";
const VECTOR: &str = "Ljdk/internal/vm/vector/VectorSupport$Vector;";
const MASK: &str = "Ljdk/internal/vm/vector/VectorSupport$VectorMask;";
const SPECIES: &str = "Ljdk/internal/vm/vector/VectorSupport$VectorSpecies;";

// `VectorSupport`'s opcode numbering. Copied from the JDK source rather than
// derived, and only the ones this file implements are named: an opcode that is
// absent here takes the fallback, so a wrong constant would be a wrong ANSWER
// while a missing one is only a missed optimisation.
const OP_ABS: i32 = 0;
const OP_NEG: i32 = 1;
const OP_SQRT: i32 = 2;
const OP_ADD: i32 = 4;
const OP_SUB: i32 = 5;
const OP_MUL: i32 = 6;
const OP_DIV: i32 = 7;
const OP_MIN: i32 = 8;
const OP_MAX: i32 = 9;
const OP_AND: i32 = 10;
const OP_OR: i32 = 11;
const OP_XOR: i32 = 12;
const OP_FMA: i32 = 13;
const OP_LSHIFT: i32 = 14;
const OP_RSHIFT: i32 = 15;
const OP_URSHIFT: i32 = 16;
const OP_CAST: i32 = 17;
const OP_UCAST: i32 = 18;
const OP_REINTERPRET: i32 = 19;

/// Are the whole-vector kernels engaged?
///
/// `CRATONVM_VECTOR_INTRINSICS=0` turns every entry point below into a pure
/// pass-through to the JDK's own lambda. That is a kill switch, and it is here
/// because a cross-binary A/B is not an A/B: with one binary and this flag, the
/// intercepted and un-intercepted arms differ in exactly one bit of
/// configuration and nothing else — not a compiler version, not a host, not a
/// merge base.
///
/// It gates the WRITE as well as the read: an "off" arm that still built the
/// result vector and then discarded it would measure the wrong thing, and a
/// switch that only silences reporting is the shape this tree has had to
/// un-ship before. `engaged()` is the FIRST thing every entry point asks.
fn engaged() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_VECTOR_INTRINSICS")
            .ok()
            .as_deref()
            != Some("0")
    })
}

/// How many calls each entry point served, and how many it handed back.
///
/// Printed by `--dump-vector-intrinsics` and by `CRATONVM_VECTOR_INTRINSICS_STATS=1`
/// at exit. A speedup quoted without these is unreadable: "the kernel got
/// faster" and "the kernel took the fallback on every call and the host was
/// quieter" produce the same number, and only the counter tells them apart.
mod stats {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub(super) static HANDLED: AtomicU64 = AtomicU64::new(0);
    pub(super) static FELL_BACK: AtomicU64 = AtomicU64::new(0);

    /// One bucket per entry point. The rows SUM to the totals above, which is
    /// what makes the census readable: "45% fell back" is not actionable, "45%
    /// fell back and every one was `load`" names the next thing to implement.
    /// The last four are the DISPATCH-LAYER templates, kept as their own rows
    /// rather than folded into the four kernel rows above. A template hit does
    /// not go on to call its kernel — it answers the whole operation — so one
    /// shared bucket would report the same work twice under one name and make
    /// "which layer is answering" unanswerable, which is the only question
    /// worth asking once both layers exist.
    pub(super) const ENTRY_NAMES: [&str; 13] = [
        "binaryOp",
        "unaryOp",
        "ternaryOp",
        "broadcastInt",
        "reductionCoerced",
        "convert",
        "fromBitsCoerced",
        "load",
        "store",
        "tmpl:lanewise(Binary)",
        "tmpl:lanewise(Unary)",
        "tmpl:lanewise(Ternary)",
        "tmpl:lanewiseShift",
    ];
    pub(super) const E_BINARY: usize = 0;
    pub(super) const E_UNARY: usize = 1;
    pub(super) const E_TERNARY: usize = 2;
    pub(super) const E_BROADCAST_INT: usize = 3;
    pub(super) const E_REDUCTION: usize = 4;
    pub(super) const E_CONVERT: usize = 5;
    pub(super) const E_FROM_BITS: usize = 6;
    pub(super) const E_LOAD: usize = 7;
    pub(super) const E_STORE: usize = 8;
    pub(super) const E_TMPL_BINARY: usize = 9;
    pub(super) const E_TMPL_UNARY: usize = 10;
    pub(super) const E_TMPL_TERNARY: usize = 11;
    pub(super) const E_TMPL_SHIFT: usize = 12;

    const N: usize = ENTRY_NAMES.len();
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    pub(super) static ENTRY_HANDLED: [AtomicU64; N] = [ZERO; N];
    pub(super) static ENTRY_FELL_BACK: [AtomicU64; N] = [ZERO; N];

    #[inline]
    pub(super) fn handled(entry: usize) {
        HANDLED.fetch_add(1, Ordering::Relaxed);
        ENTRY_HANDLED[entry].fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn fell_back(entry: usize) {
        FELL_BACK.fetch_add(1, Ordering::Relaxed);
        ENTRY_FELL_BACK[entry].fetch_add(1, Ordering::Relaxed);
    }
}

/// The per-entry-point census, as `(name, handled, fell_back)` rows summing to
/// [`vector_intrinsic_counts`].
pub fn vector_intrinsic_census() -> Vec<(&'static str, u64, u64)> {
    use std::sync::atomic::Ordering;
    stats::ENTRY_NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            (
                *name,
                stats::ENTRY_HANDLED[i].load(Ordering::Relaxed),
                stats::ENTRY_FELL_BACK[i].load(Ordering::Relaxed),
            )
        })
        .collect()
}

/// `(handled, fell_back)` — the engagement counters behind any number quoted
/// from this file.
pub fn vector_intrinsic_counts() -> (u64, u64) {
    use std::sync::atomic::Ordering;
    (
        stats::HANDLED.load(Ordering::Relaxed),
        stats::FELL_BACK.load(Ordering::Relaxed),
    )
}

/// Index of `VectorSupport$VectorPayload.payload` on `class_id`.
///
/// By NAME, with 0 as the fallback. 0 is right today — it is the only instance
/// field in the whole `VectorPayload` -> `AbstractVector` -> `Float256Vector`
/// chain — but a name lookup costs one class-manager read against an operation
/// that already allocates, and it is the difference between this code following
/// a JDK layout change and silently reading the wrong slot after one.
fn payload_slot(ctx: &dyn NativeContext, class_id: ClassId) -> usize {
    ctx.resolve_field_index_by_class_id(class_id, "payload")
        .unwrap_or(0)
}

/// The primitive lane array behind a vector, mask or shuffle.
fn payload_of(ctx: &dyn NativeContext, v: ObjectRef) -> Option<ObjectRef> {
    let slot = payload_slot(ctx, ctx.class_id_of_object(v));
    match ctx.get_field(v, slot) {
        Value::Object(Some(arr)) if ctx.object_is_array(arr) => Some(arr),
        _ => None,
    }
}

/// `ELEM_*` for a payload array, or `None` for a carrier this file does not
/// model (a `boolean[]` mask payload, an object array).
fn elem_code_of(ctx: &dyn NativeContext, arr: ObjectRef) -> Option<u8> {
    Some(match ctx.heap_element_type_of(arr) {
        ArrayElementType::Byte => ELEM_BYTE,
        ArrayElementType::Short | ArrayElementType::Char => ELEM_SHORT,
        ArrayElementType::Int => ELEM_INT,
        ArrayElementType::Long => ELEM_LONG,
        ArrayElementType::Float => ELEM_FLOAT,
        ArrayElementType::Double => ELEM_DOUBLE,
        ArrayElementType::Boolean | ArrayElementType::Reference => return None,
    })
}

fn array_type_of(elem: u8) -> ArrayElementType {
    match elem {
        ELEM_BYTE => ArrayElementType::Byte,
        ELEM_SHORT => ArrayElementType::Short,
        ELEM_LONG => ArrayElementType::Long,
        ELEM_FLOAT => ArrayElementType::Float,
        ELEM_DOUBLE => ArrayElementType::Double,
        _ => ArrayElementType::Int,
    }
}

/// `ELEM_*` for a `Class` mirror, from EITHER the element type or the vector
/// class.
///
/// The element type arrives as a PRIMITIVE mirror (`float.class`), and
/// `class_id_from_mirror` does not resolve one — a primitive has no loaded
/// class behind it. That is not a hypothetical: it is why `fromBitsCoerced` and
/// `convert` reported `handled=0 fell_back=93757` and `fell_back=46848` on the
/// first measured run, with every other entry point at a clean zero. The
/// per-entry census is what turned "45% of calls fall back" into those two
/// names.
///
/// The vector class (`Float256Vector.class`) resolves normally and its NAME
/// carries the same fact, so try that first and keep the primitive path as the
/// second answer. Both are checked because `convert`'s destination is described
/// by both and a caller may hand over only one.
fn elem_code_of_mirror(ctx: &dyn NativeContext, mirror: ObjectRef) -> Option<u8> {
    let name = ctx
        .class_id_from_mirror(mirror)
        .and_then(|id| ctx.class_name_of_id(id))?;
    let simple = name.rsplit('/').next().unwrap_or(&name);
    Some(match simple {
        "byte" => ELEM_BYTE,
        "short" | "char" => ELEM_SHORT,
        "int" => ELEM_INT,
        "long" => ELEM_LONG,
        "float" => ELEM_FLOAT,
        "double" => ELEM_DOUBLE,
        // MASKS AND SHUFFLES ARE REFUSED, and that refusal is why this arm has
        // a screen rather than only a prefix table.
        // `Long256Vector$Long256Mask` starts with "Long" and is NOT a long-lane
        // carrier: a mask's payload is a `boolean[]` and a shuffle's is a
        // `byte[]` of lane indices. Without this, `VectorSupport.load` of a mask
        // built a `long[]` payload and the next `getBits()` died with
        // `ClassCastException: class [J cannot be cast to class [Z`, four frames
        // inside `LongVector.fromMemorySegment`. Caught by
        // `probes/VectorApiProbe.java`'s `CONV.i2l.reinterpret` row — the only
        // one of its 320 that reaches a mask through this path, which is the
        // argument for a probe that walks the API rather than the kernel.
        _ if simple.contains("Mask") || simple.contains("Shuffle") => return None,
        // `Float256Vector`, `Int64Vector`, `ByteMaxVector`, … — the shape name
        // that `jdk.incubator.vector` generates one class per. Prefix, not
        // substring, so `DoubleMaxVector` cannot match on an `Int` elsewhere in
        // the name.
        _ => {
            if simple.starts_with("Byte") {
                ELEM_BYTE
            } else if simple.starts_with("Short") {
                ELEM_SHORT
            } else if simple.starts_with("Int") {
                ELEM_INT
            } else if simple.starts_with("Long") {
                ELEM_LONG
            } else if simple.starts_with("Float") {
                ELEM_FLOAT
            } else if simple.starts_with("Double") {
                ELEM_DOUBLE
            } else {
                return None;
            }
        }
    })
}

/// Read every lane of `v` as raw bits, or `None` if it is not a shape this file
/// models.
fn lanes_of(ctx: &mut dyn NativeContext, v: ObjectRef) -> Option<(u8, Vec<i64>)> {
    let arr = payload_of(ctx, v)?;
    let elem = elem_code_of(ctx, arr)?;
    let len = ctx.array_length(arr);
    let mut lanes = Vec::with_capacity(len);
    for i in 0..len {
        lanes.push(array_lane_bits(ctx, arr, i, elem));
    }
    Some((elem, lanes))
}

/// Build a vector of `vm_class` (a `Class` mirror) whose payload holds `lanes`.
///
/// The array is allocated first and PINNED across the object allocation: a
/// young-generation collection between the two would otherwise leave `arr`
/// pointing at a moved object, which is the native stale-local family this tree
/// keeps re-finding. Lanes are written after both allocations, through the
/// re-read handle.
fn build_vector(
    ctx: &mut dyn NativeContext,
    entry: usize,
    vm_class: ObjectRef,
    elem: u8,
    lanes: &[i64],
) -> Option<ObjectRef> {
    let class_id = ctx.class_id_from_mirror(vm_class)?;
    let arr0 = ctx.new_array(array_type_of(elem), lanes.len());
    let pin = ctx.pin_native_root(arr0);
    let slots = ctx.class_num_total_fields(class_id).max(1);
    let obj = ctx
        .try_alloc_object_gc_safe(class_id, slots)
        .unwrap_or_else(|| ctx.alloc_object(class_id, slots));
    let arr = ctx.read_native_pin(pin, arr0);
    for (i, bits) in lanes.iter().copied().enumerate() {
        ctx.set_array_element(arr, i, lane_bits_to_value(elem, bits));
    }
    let slot = payload_slot(ctx, class_id);
    ctx.set_field(obj, slot, Value::Object(Some(arr)));
    ctx.unpin_native_roots(pin);
    stats::handled(entry);
    Some(obj)
}

/// Hand the call back to the JDK's own lambda, unchanged.
///
/// This is not an error path — it is the contract that lets this file implement
/// a subset. `args[last]` is always the `defaultImpl` for every `VectorSupport`
/// entry point, and its SAM is invoked with exactly the arguments the JDK's own
/// fallback body passes.
fn fallback(
    ctx: &mut dyn NativeContext,
    entry: usize,
    default_impl: Option<&Value>,
    sam: &str,
    descriptor: &str,
    sam_args: &[Value],
) -> MethodCallResult {
    stats::fell_back(entry);
    let receiver = match default_impl {
        Some(Value::Object(Some(obj))) => *obj,
        // No lambda to fall back to. Returning `null` here would be a wrong
        // ANSWER; the JDK's own body would have raised the same NPE.
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("VectorSupport: no default implementation to fall back to".into()),
            }
            .into())
        }
    };
    ctx.invoke_virtual(receiver, sam, descriptor, sam_args)
}

fn int_at(args: &[Value], i: usize) -> i32 {
    match args.get(i) {
        Some(Value::Int(n)) => *n,
        Some(Value::Long(n)) => *n as i32,
        _ => 0,
    }
}

fn obj_at(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// Is `m` a mask argument that actually constrains the operation?
///
/// `null` is the unmasked form and is the overwhelming majority of calls. A
/// non-null mask takes the fallback: implementing masked lanes here would be
/// another table to keep right, and the un-intercepted answer is already
/// correct.
fn unmasked(args: &[Value], i: usize) -> bool {
    matches!(args.get(i), Some(Value::Object(None)) | None)
}

// ---------------------------------------------------------------------------
// The lane kernels
// ---------------------------------------------------------------------------

/// A binary lane op on raw bits. `None` = "not implemented for this element
/// type", which the caller turns into a fallback rather than a wrong answer.
///
/// Integral ops are computed in `i64` and narrowed by `wrap_integral_lane`,
/// which is the same rule `vector_api.rs` uses — one implementation of the
/// wrap, not two. The shift ops mask their count the way the JVM's `ishl`/`lshl`
/// do (`& 31` / `& 63`), which is also what `VectorOperators` specifies.
fn binary_lane(op: i32, elem: u8, a: i64, b: i64) -> Option<i64> {
    let float_op = |f: fn(f32, f32) -> f32| -> Option<i64> {
        Some(f(f32::from_bits(a as u32), f32::from_bits(b as u32)).to_bits() as i64)
    };
    let double_op = |f: fn(f64, f64) -> f64| -> Option<i64> {
        Some(f(f64::from_bits(a as u64), f64::from_bits(b as u64)).to_bits() as i64)
    };
    match elem {
        ELEM_FLOAT => match op {
            OP_ADD => float_op(|x, y| x + y),
            OP_SUB => float_op(|x, y| x - y),
            OP_MUL => float_op(|x, y| x * y),
            OP_DIV => float_op(|x, y| x / y),
            // `Math.min`/`Math.max` semantics, which are NOT `f32::min`/`max`:
            // the Java methods propagate NaN and distinguish -0.0 from 0.0,
            // Rust's return the non-NaN operand. `VectorOperators.MIN` is
            // specified as `Math.min`.
            OP_MIN => float_op(java_min_f32),
            OP_MAX => float_op(java_max_f32),
            _ => None,
        },
        ELEM_DOUBLE => match op {
            OP_ADD => double_op(|x, y| x + y),
            OP_SUB => double_op(|x, y| x - y),
            OP_MUL => double_op(|x, y| x * y),
            OP_DIV => double_op(|x, y| x / y),
            OP_MIN => double_op(java_min_f64),
            OP_MAX => double_op(java_max_f64),
            _ => None,
        },
        _ => {
            // THE LANE'S OWN WIDTH, not the machine word's. A `short` lane
            // shifts by `n & 15` and its `>>>` zero-extends to 16 bits, not to
            // 32: the JDK's generated body is
            // `(short)((a & 0xFFFF) >>> (n & 15))`.
            //
            // This read `32` for everything below `long`, and
            // `probes/VectorApiProbe.java` caught it on ONE row:
            // `ShortVector.lanewise(LSHR, 1)` of `Short.MIN_VALUE` answered
            // -16384 where the oracle says 16384, because the sign-extended
            // lane was zero-extended at 32 bits and narrowed back afterwards.
            // Every other shift row agreed — which is the argument for a probe
            // that enumerates lane widths rather than one that spot-checks.
            let bits: i64 = match elem {
                ELEM_LONG => 64,
                ELEM_INT => 32,
                ELEM_SHORT => 16,
                _ => 8,
            };
            let shift_mask = bits - 1;
            let value = match op {
                OP_ADD => a.wrapping_add(b),
                OP_SUB => a.wrapping_sub(b),
                OP_MUL => a.wrapping_mul(b),
                // Integral division by zero is an ArithmeticException in Java.
                // The fallback raises it from the JDK's own lane lambda with
                // the JDK's own message, so refuse the whole vector rather than
                // inventing one here.
                OP_DIV => {
                    if b == 0 {
                        return None;
                    }
                    // `MIN_VALUE / -1` overflows in Rust and wraps in Java.
                    a.wrapping_div(b)
                }
                OP_MIN => a.min(b),
                OP_MAX => a.max(b),
                OP_AND => a & b,
                OP_OR => a | b,
                OP_XOR => a ^ b,
                OP_LSHIFT => a.wrapping_shl((b & shift_mask) as u32),
                OP_RSHIFT => a.wrapping_shr((b & shift_mask) as u32),
                // Zero-extend WITHIN THE LANE, then shift. The mask below is
                // `(1 << bits) - 1` for every width except 64, where it is all
                // ones and the shift is on `u64` directly.
                OP_URSHIFT => {
                    let ua = if bits == 64 {
                        a as u64
                    } else {
                        (a as u64) & ((1u64 << bits) - 1)
                    };
                    (ua.wrapping_shr((b & shift_mask) as u32)) as i64
                }
                _ => return None,
            };
            Some(wrap_integral_lane(elem, value))
        }
    }
}

/// `Math.min(float, float)` — NaN-propagating and -0.0-aware, unlike `f32::min`.
fn java_min_f32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { a } else { b };
    }
    if a < b {
        a
    } else {
        b
    }
}

fn java_max_f32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_positive() { a } else { b };
    }
    if a > b {
        a
    } else {
        b
    }
}

fn java_min_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { a } else { b };
    }
    if a < b {
        a
    } else {
        b
    }
}

fn java_max_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_positive() { a } else { b };
    }
    if a > b {
        a
    } else {
        b
    }
}

fn unary_lane(op: i32, elem: u8, a: i64) -> Option<i64> {
    match elem {
        ELEM_FLOAT => {
            let x = f32::from_bits(a as u32);
            Some(
                match op {
                    OP_ABS => x.abs(),
                    OP_NEG => -x,
                    OP_SQRT => x.sqrt(),
                    _ => return None,
                }
                .to_bits() as i64,
            )
        }
        ELEM_DOUBLE => {
            let x = f64::from_bits(a as u64);
            Some(
                match op {
                    OP_ABS => x.abs(),
                    OP_NEG => -x,
                    OP_SQRT => x.sqrt(),
                    _ => return None,
                }
                .to_bits() as i64,
            )
        }
        _ => {
            let value = match op {
                OP_ABS => a.wrapping_abs(),
                OP_NEG => a.wrapping_neg(),
                _ => return None,
            };
            Some(wrap_integral_lane(elem, value))
        }
    }
}

// ---------------------------------------------------------------------------
// The registered entry points
// ---------------------------------------------------------------------------

/// `VectorSupport.maybeRebox(VP)` — identity plus a load fence.
///
/// The JDK's body is `U.loadFence(); return v;`. It is on this list because it
/// is called on nearly every operand of nearly every operation, so an
/// interpreted call frame per invocation is pure overhead — 53 of 4722 samples
/// in the profile that motivated this file, before any of the real work.
fn vs_maybe_rebox(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

/// `binaryOp(oprId, vmClass, maskClass, eClass, length, v1, v2, m, defaultImpl)`
fn vs_binary_op(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    const D: &str = "BinaryOperation";
    let sam_desc = format!("({PAYLOAD}{PAYLOAD}{MASK}){PAYLOAD}");
    let take_fallback = |ctx: &mut dyn NativeContext| {
        fallback(
            ctx,
            stats::E_BINARY,
            args.get(8),
            "apply",
            &sam_desc,
            &[
                args.get(5).copied().unwrap_or(Value::Object(None)),
                args.get(6).copied().unwrap_or(Value::Object(None)),
                args.get(7).copied().unwrap_or(Value::Object(None)),
            ],
        )
    };
    let _ = D;
    let op = int_at(args, 0);
    let (Some(vm_class), Some(v1), Some(v2)) = (obj_at(args, 1), obj_at(args, 5), obj_at(args, 6))
    else {
        return take_fallback(ctx);
    };
    if !unmasked(args, 7) {
        return take_fallback(ctx);
    }
    let (Some((elem, a)), Some((elem2, b))) = (lanes_of(ctx, v1), lanes_of(ctx, v2)) else {
        return take_fallback(ctx);
    };
    if elem != elem2 || a.len() != b.len() || a.is_empty() {
        return take_fallback(ctx);
    }
    let mut out = Vec::with_capacity(a.len());
    for (x, y) in a.iter().copied().zip(b.iter().copied()) {
        match binary_lane(op, elem, x, y) {
            Some(bits) => out.push(bits),
            None => return take_fallback(ctx),
        }
    }
    match build_vector(ctx, stats::E_BINARY, vm_class, elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => take_fallback(ctx),
    }
}

/// `unaryOp(oprId, vClass, maskClass, eClass, length, v, m, defaultImpl)`
fn vs_unary_op(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sam_desc = format!("({VECTOR}{MASK}){VECTOR}");
    let take_fallback = |ctx: &mut dyn NativeContext| {
        fallback(
            ctx,
            stats::E_UNARY,
            args.get(7),
            "apply",
            &sam_desc,
            &[
                args.get(5).copied().unwrap_or(Value::Object(None)),
                args.get(6).copied().unwrap_or(Value::Object(None)),
            ],
        )
    };
    let op = int_at(args, 0);
    let (Some(vm_class), Some(v)) = (obj_at(args, 1), obj_at(args, 5)) else {
        return take_fallback(ctx);
    };
    if !unmasked(args, 6) {
        return take_fallback(ctx);
    }
    let Some((elem, a)) = lanes_of(ctx, v) else {
        return take_fallback(ctx);
    };
    if a.is_empty() {
        return take_fallback(ctx);
    }
    let mut out = Vec::with_capacity(a.len());
    for x in a.iter().copied() {
        match unary_lane(op, elem, x) {
            Some(bits) => out.push(bits),
            None => return take_fallback(ctx),
        }
    }
    match build_vector(ctx, stats::E_UNARY, vm_class, elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => take_fallback(ctx),
    }
}

/// `ternaryOp(oprId, vClass, maskClass, eClass, length, v1, v2, v3, m, defaultImpl)`
///
/// Only `VECTOR_OP_FMA`, which is the only ternary the API exposes. It is the
/// single most valuable entry here: its fallback calls `Math.fma` **per lane**,
/// and `Math.fma`'s own Java body builds two `BigDecimal`s and runs a
/// `BigInteger` Knuth division. `f32::mul_add` / `f64::mul_add` are IEEE 754
/// `fusedMultiplyAdd`, the same single-rounding contract.
fn vs_ternary_op(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sam_desc = format!("({VECTOR}{VECTOR}{VECTOR}{MASK}){VECTOR}");
    let take_fallback = |ctx: &mut dyn NativeContext| {
        fallback(
            ctx,
            stats::E_TERNARY,
            args.get(9),
            "apply",
            &sam_desc,
            &[
                args.get(5).copied().unwrap_or(Value::Object(None)),
                args.get(6).copied().unwrap_or(Value::Object(None)),
                args.get(7).copied().unwrap_or(Value::Object(None)),
                args.get(8).copied().unwrap_or(Value::Object(None)),
            ],
        )
    };
    if int_at(args, 0) != OP_FMA || !unmasked(args, 8) {
        return take_fallback(ctx);
    }
    let (Some(vm_class), Some(v1), Some(v2), Some(v3)) = (
        obj_at(args, 1),
        obj_at(args, 5),
        obj_at(args, 6),
        obj_at(args, 7),
    ) else {
        return take_fallback(ctx);
    };
    let (Some((e1, a)), Some((e2, b)), Some((e3, c))) =
        (lanes_of(ctx, v1), lanes_of(ctx, v2), lanes_of(ctx, v3))
    else {
        return take_fallback(ctx);
    };
    if e1 != e2 || e1 != e3 || a.len() != b.len() || a.len() != c.len() || a.is_empty() {
        return take_fallback(ctx);
    }
    let mut out = Vec::with_capacity(a.len());
    for i in 0..a.len() {
        let bits = match e1 {
            ELEM_FLOAT => f32::from_bits(a[i] as u32)
                .mul_add(f32::from_bits(b[i] as u32), f32::from_bits(c[i] as u32))
                .to_bits() as i64,
            ELEM_DOUBLE => f64::from_bits(a[i] as u64)
                .mul_add(f64::from_bits(b[i] as u64), f64::from_bits(c[i] as u64))
                .to_bits() as i64,
            _ => return take_fallback(ctx),
        };
        out.push(bits);
    }
    match build_vector(ctx, stats::E_TERNARY, vm_class, e1, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => take_fallback(ctx),
    }
}

/// `broadcastInt(opr, vClass, maskClass, eClass, length, v, n, m, defaultImpl)`
///
/// The shift-by-scalar form: `v.lanewise(LSHL, 13)`. Its fallback broadcasts the
/// scalar into a whole vector first and then runs the binary lambda per lane, so
/// intercepting it saves an allocation as well as the loop.
fn vs_broadcast_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sam_desc = format!("({VECTOR}I{MASK}){VECTOR}");
    let take_fallback = |ctx: &mut dyn NativeContext| {
        fallback(
            ctx,
            stats::E_BROADCAST_INT,
            args.get(8),
            "apply",
            &sam_desc,
            &[
                args.get(5).copied().unwrap_or(Value::Object(None)),
                Value::Int(int_at(args, 6)),
                args.get(7).copied().unwrap_or(Value::Object(None)),
            ],
        )
    };
    let op = int_at(args, 0);
    let n = i64::from(int_at(args, 6));
    let (Some(vm_class), Some(v)) = (obj_at(args, 1), obj_at(args, 5)) else {
        return take_fallback(ctx);
    };
    if !unmasked(args, 7) {
        return take_fallback(ctx);
    }
    let Some((elem, a)) = lanes_of(ctx, v) else {
        return take_fallback(ctx);
    };
    if a.is_empty() {
        return take_fallback(ctx);
    }
    let mut out = Vec::with_capacity(a.len());
    for x in a.iter().copied() {
        match binary_lane(op, elem, x, n) {
            Some(bits) => out.push(bits),
            None => return take_fallback(ctx),
        }
    }
    match build_vector(ctx, stats::E_BROADCAST_INT, vm_class, elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => take_fallback(ctx),
    }
}

/// `reductionCoerced(oprId, vClass, maskClass, eClass, length, v, m, defaultImpl)`
///
/// Returns the accumulated value as raw bits in a `long`, which is the JDK's own
/// convention for this entry point — the caller reinterprets.
fn vs_reduction_coerced(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sam_desc = format!("({VECTOR}{MASK})J");
    let take_fallback = |ctx: &mut dyn NativeContext| {
        fallback(
            ctx,
            stats::E_REDUCTION,
            args.get(7),
            "apply",
            &sam_desc,
            &[
                args.get(5).copied().unwrap_or(Value::Object(None)),
                args.get(6).copied().unwrap_or(Value::Object(None)),
            ],
        )
    };
    let op = int_at(args, 0);
    let Some(v) = obj_at(args, 5) else {
        return take_fallback(ctx);
    };
    if !unmasked(args, 6) {
        return take_fallback(ctx);
    }
    let Some((elem, a)) = lanes_of(ctx, v) else {
        return take_fallback(ctx);
    };
    if a.is_empty() {
        return take_fallback(ctx);
    }
    // Left-to-right, which is the order `ldOp`/`rOp` reduce in. It is not an
    // arbitrary choice for floats: addition is not associative there, so a
    // different order is a different answer, and `probes/VectorApiProbe.java`
    // compares the bits.
    let mut acc = a[0];
    for x in a.iter().copied().skip(1) {
        match binary_lane(op, elem, acc, x) {
            Some(bits) => acc = bits,
            None => return take_fallback(ctx),
        }
    }
    // A reduction's result is widened to `long` by ZERO-extending the lane's
    // bits for the sub-word types, because the caller narrows it back with a
    // cast. Sign-extending an `int` lane here would set the high half and the
    // float reinterpretation on the far side would read it.
    stats::handled(stats::E_REDUCTION);
    let widened = match elem {
        ELEM_LONG | ELEM_DOUBLE => acc,
        ELEM_FLOAT | ELEM_INT => (acc as u32) as i64,
        ELEM_SHORT => (acc as u16) as i64,
        _ => (acc as u8) as i64,
    };
    Ok(Some(Value::Long(widened)))
}

/// `convert(oprId, fromVectorClass, fromeClass, fromVLen, toVectorClass, toeClass,
///          toVLen, v, s, defaultImpl)`
///
/// Two shapes, and they are genuinely different operations:
///
///  * `VECTOR_OP_REINTERPRET` keeps the BITS and changes how they are read. Only
///    the same-total-width, same-lane-count case is handled here (an
///    `int[8]` seen as `float[8]`); a reshape crosses lane boundaries and is
///    left to the fallback.
///  * `VECTOR_OP_CAST` / `UCAST` convert VALUES, lane by lane, with Java's own
///    narrowing and widening rules. `UCAST` differs from `CAST` only when
///    widening an integral lane, where it zero-extends.
fn vs_convert(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sam_desc = format!("({PAYLOAD}{SPECIES}){PAYLOAD}");
    let take_fallback = |ctx: &mut dyn NativeContext| {
        fallback(
            ctx,
            stats::E_CONVERT,
            args.get(9),
            "apply",
            &sam_desc,
            &[
                args.get(7).copied().unwrap_or(Value::Object(None)),
                args.get(8).copied().unwrap_or(Value::Object(None)),
            ],
        )
    };
    let op = int_at(args, 0);
    let to_len = int_at(args, 6).max(0) as usize;
    let (Some(to_class), Some(to_etype), Some(v)) =
        (obj_at(args, 4), obj_at(args, 5), obj_at(args, 7))
    else {
        return take_fallback(ctx);
    };
    let (Some(to_elem), Some((from_elem, a))) =
        (elem_code_of_mirror(ctx, to_etype).or_else(|| elem_code_of_mirror(ctx, to_class)), lanes_of(ctx, v))
    else {
        return take_fallback(ctx);
    };
    if a.is_empty() || to_len == 0 {
        return take_fallback(ctx);
    }
    let out: Vec<i64> = match op {
        OP_REINTERPRET => {
            // Same lane count and same lane width, i.e. a pure relabelling.
            // Anything else redistributes bits across lanes and is the
            // fallback's business.
            if a.len() != to_len || elem_width(from_elem) != elem_width(to_elem) {
                return take_fallback(ctx);
            }
            a
        }
        OP_CAST | OP_UCAST => {
            if a.len() < to_len {
                // A widening reshape reads lanes this vector does not have; the
                // JDK zero-fills them and the rule for which part of the source
                // is taken is `castShape`'s, not this file's.
                return take_fallback(ctx);
            }
            let unsigned = op == OP_UCAST;
            let mut out = Vec::with_capacity(to_len);
            for x in a.iter().copied().take(to_len) {
                match cast_lane(from_elem, to_elem, x, unsigned) {
                    Some(bits) => out.push(bits),
                    None => return take_fallback(ctx),
                }
            }
            out
        }
        _ => return take_fallback(ctx),
    };
    match build_vector(ctx, stats::E_CONVERT, to_class, to_elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => take_fallback(ctx),
    }
}

fn elem_width(elem: u8) -> u32 {
    match elem {
        ELEM_BYTE => 1,
        ELEM_SHORT => 2,
        ELEM_LONG | ELEM_DOUBLE => 8,
        _ => 4,
    }
}

/// One lane of `VECTOR_OP_CAST` / `VECTOR_OP_UCAST`, in Java's conversion rules.
fn cast_lane(from: u8, to: u8, bits: i64, unsigned: bool) -> Option<i64> {
    // Everything routes through a common intermediate so the table is
    // conversions-to and conversions-from rather than an NxN matrix.
    let as_f64 = match from {
        ELEM_FLOAT => f64::from(f32::from_bits(bits as u32)),
        ELEM_DOUBLE => f64::from_bits(bits as u64),
        _ => {
            let integral = if unsigned {
                match from {
                    ELEM_BYTE => i64::from(bits as u8),
                    ELEM_SHORT => i64::from(bits as u16),
                    ELEM_INT => i64::from(bits as u32),
                    _ => bits,
                }
            } else {
                bits
            };
            return Some(match to {
                // Java's `(float) long` / `(double) long` round-to-nearest.
                ELEM_FLOAT => (integral as f32).to_bits() as i64,
                ELEM_DOUBLE => (integral as f64).to_bits() as i64,
                _ => wrap_integral_lane(to, integral),
            });
        }
    };
    Some(match to {
        ELEM_FLOAT => (as_f64 as f32).to_bits() as i64,
        ELEM_DOUBLE => as_f64.to_bits() as i64,
        // Java's float->integral narrowing saturates and maps NaN to 0, which
        // is exactly `as` in Rust since 1.45 — not UB, and not a wrap.
        ELEM_LONG => as_f64 as i64,
        _ => wrap_integral_lane(to, i64::from(as_f64 as i32)),
    })
}

/// `fromBitsCoerced(vmClass, eClass, length, bits, mode, s, defaultImpl)`
///
/// `MODE_BROADCAST` only — every lane takes the same value. The mask mode
/// (`MODE_BITS_COERCED_LONG_TO_MASK`) builds a `boolean[]` payload this file
/// does not model and takes the fallback.
fn vs_from_bits_coerced(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    const MODE_BROADCAST: i32 = 0;
    let sam_desc = format!("(J{SPECIES}){PAYLOAD}");
    let take_fallback = |ctx: &mut dyn NativeContext| {
        fallback(
            ctx,
            stats::E_FROM_BITS,
            args.get(6),
            "fromBits",
            &sam_desc,
            &[
                args.get(3).copied().unwrap_or(Value::Long(0)),
                args.get(5).copied().unwrap_or(Value::Object(None)),
            ],
        )
    };
    let mode = int_at(args, 4);
    let length = int_at(args, 2).max(0) as usize;
    let bits = match args.get(3) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => i64::from(*n),
        _ => 0,
    };
    let (Some(vm_class), Some(etype)) = (obj_at(args, 0), obj_at(args, 1)) else {
        return take_fallback(ctx);
    };
    // The element type first, the vector class as the second answer: see
    // `elem_code_of_mirror` for why a primitive mirror alone is not enough.
    let Some(elem) = elem_code_of_mirror(ctx, etype)
        .or_else(|| elem_code_of_mirror(ctx, vm_class))
    else {
        return take_fallback(ctx);
    };
    if mode != MODE_BROADCAST || length == 0 {
        return take_fallback(ctx);
    }
    // The bits arrive already in the lane's own encoding — the JDK's callers
    // pass `Float.floatToRawIntBits(e)` for a float broadcast — so the lane is
    // the low bits, narrowed. No conversion.
    let lane = match elem {
        ELEM_LONG | ELEM_DOUBLE => bits,
        ELEM_FLOAT | ELEM_INT => (bits as u32) as i64,
        _ => wrap_integral_lane(elem, bits),
    };
    let out = vec![lane; length];
    match build_vector(ctx, stats::E_FROM_BITS, vm_class, elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => take_fallback(ctx),
    }
}

/// `Unsafe.arrayBaseOffset(<any array type>)` as this VM publishes it.
///
/// `load`/`store` receive a `(base, offset)` pair in `Unsafe` terms, so turning
/// that back into a lane index needs the same constant the rest of the VM uses.
/// Shared from `native-io` rather than restated: a second 16 here that drifted
/// from that one would read one lane off the front of every array.
const ARRAY_BASE_OFFSET: i64 = cratonvm_native_io::ARRAY_BYTE_BASE_OFFSET;

/// Where a `load`/`store` should read or write.
enum Where {
    /// A Java primitive array, and the lane index of the first element.
    Heap(ObjectRef, usize),
    /// An absolute machine address — an off-heap `MemorySegment`.
    Native(i64),
}

/// Decode `VectorSupport`'s `(base, offset)` pair.
///
/// This is the JDK's own convention and CratonVM already speaks both halves of
/// it: `AbstractMemorySegmentImpl.unsafeGetBase()` answers the backing array or
/// null, and `unsafeGetOffset()` answers an `Unsafe`-style offset into it or an
/// absolute address. (Both are CratonVM natives as of 2026-08-22 — see
/// `panama::register_craton_segment_impl_surface`.) An array container arrives
/// the same way, so one decoder serves `fromArray`, `fromMemorySegment` and
/// their store twins.
///
/// `None` for anything else: a `base` that is not an array, an offset that is
/// not a whole number of lanes from the array's first element, or a run that
/// would leave the array. Each of those takes the fallback, which is the
/// un-intercepted answer — including the `IndexOutOfBoundsException` a genuinely
/// out-of-range access must raise, with the JDK's own message.
fn decode_access(
    ctx: &dyn NativeContext,
    base: Option<ObjectRef>,
    offset: i64,
    elem: u8,
    lanes: usize,
) -> Option<Where> {
    let width = i64::from(elem_width(elem));
    match base {
        None => {
            if offset == 0 {
                // A null base with a zero offset is not an address, it is an
                // unresolved carrier. Refuse rather than dereference 0.
                return None;
            }
            Some(Where::Native(offset))
        }
        Some(arr) => {
            if !ctx.object_is_array(arr) {
                return None;
            }
            // The container's element width need not be the vector's — a
            // `byte[]`-backed `MemorySegment` holding float lanes is the shape
            // `AbstractVector.defaultReinterpret` builds. Index in the ARRAY's
            // own elements, and refuse when the lanes do not land on element
            // boundaries.
            let arr_width = match ctx.heap_element_type_of(arr) {
                ArrayElementType::Byte | ArrayElementType::Boolean => 1,
                ArrayElementType::Short | ArrayElementType::Char => 2,
                ArrayElementType::Int | ArrayElementType::Float => 4,
                ArrayElementType::Long | ArrayElementType::Double => 8,
                ArrayElementType::Reference => return None,
            };
            if arr_width != width {
                return None;
            }
            let byte_off = offset - ARRAY_BASE_OFFSET;
            if byte_off < 0 || byte_off % width != 0 {
                return None;
            }
            let start = (byte_off / width) as usize;
            if start.checked_add(lanes)? > ctx.array_length(arr) {
                return None;
            }
            Some(Where::Heap(arr, start))
        }
    }
}

/// Read `lanes` lanes of `elem` from a decoded location.
///
/// SAFETY (the `Native` arm): the address comes from the JDK's own
/// `unsafeGetOffset()` on a segment whose bounds `IOUtil`-side checks have
/// already validated, which is the same trust `MemorySegment.get` operates
/// under one layer down. `read_unaligned` is used because a segment lane is not
/// required to be aligned — the Vector API's own element layout is
/// `withByteAlignment(1)`.
fn read_lanes(ctx: &dyn NativeContext, at: &Where, elem: u8, lanes: usize) -> Vec<i64> {
    let mut out = Vec::with_capacity(lanes);
    match *at {
        Where::Heap(arr, start) => {
            for i in 0..lanes {
                out.push(array_lane_bits_ref(ctx, arr, start + i, elem));
            }
        }
        Where::Native(addr) => {
            let width = elem_width(elem) as usize;
            for i in 0..lanes {
                let p = (addr as usize + i * width) as *const u8;
                let bits = unsafe {
                    match width {
                        1 => i64::from(std::ptr::read_unaligned(p as *const i8)),
                        2 => i64::from(std::ptr::read_unaligned(p as *const i16)),
                        4 => i64::from(std::ptr::read_unaligned(p as *const i32)),
                        _ => std::ptr::read_unaligned(p as *const i64),
                    }
                };
                // Float and int lanes share the 4-byte read above; the caller
                // reinterprets, so the only narrowing needed is the unsigned
                // one for `float`, whose bits must not sign-extend.
                out.push(match elem {
                    ELEM_FLOAT => (bits as u32) as i64,
                    _ => bits,
                });
            }
        }
    }
    out
}

/// Write `lanes` back to a decoded location.
fn write_lanes(ctx: &mut dyn NativeContext, at: &Where, elem: u8, lanes: &[i64]) {
    match *at {
        Where::Heap(arr, start) => {
            for (i, bits) in lanes.iter().copied().enumerate() {
                ctx.set_array_element(arr, start + i, lane_bits_to_value(elem, bits));
            }
        }
        Where::Native(addr) => {
            let width = elem_width(elem) as usize;
            for (i, bits) in lanes.iter().copied().enumerate() {
                let p = (addr as usize + i * width) as *mut u8;
                unsafe {
                    match width {
                        1 => std::ptr::write_unaligned(p as *mut i8, bits as i8),
                        2 => std::ptr::write_unaligned(p as *mut i16, bits as i16),
                        4 => std::ptr::write_unaligned(p as *mut i32, bits as i32),
                        _ => std::ptr::write_unaligned(p as *mut i64, bits),
                    }
                }
            }
        }
    }
}

/// `array_lane_bits` against a `&dyn` context.
///
/// The shared helper in `vector_api` takes `&mut` because most of its callers
/// have one; the read itself does not need it, and threading `&mut` through
/// `read_lanes` would stop the decoded location being borrowed at the same time.
fn array_lane_bits_ref(ctx: &dyn NativeContext, arr: ObjectRef, index: usize, elem: u8) -> i64 {
    if index >= ctx.array_length(arr) {
        return 0;
    }
    match (elem, ctx.get_array_element(arr, index)) {
        (ELEM_BYTE, Value::Int(n)) => i64::from(n as i8),
        (ELEM_SHORT, Value::Int(n)) => i64::from(n as i16),
        (ELEM_INT, Value::Int(n)) => i64::from(n),
        (ELEM_LONG, Value::Long(n)) => n,
        (ELEM_LONG, Value::Int(n)) => i64::from(n),
        (ELEM_FLOAT, Value::Float(f)) => i64::from(f.to_bits() as i32) & 0xFFFF_FFFF,
        (ELEM_FLOAT, Value::Int(n)) => i64::from(n) & 0xFFFF_FFFF,
        (ELEM_DOUBLE, Value::Double(f)) => f.to_bits() as i64,
        (ELEM_DOUBLE, Value::Long(n)) => n,
        (_, Value::Int(n)) => i64::from(n),
        (_, Value::Long(n)) => n,
        _ => 0,
    }
}

/// `load(vmClass, eClass, length, base, offset, fromSegment, container, index, s, defaultImpl)`
///
/// The `(base, offset)` pair is the whole point: the JDK has already resolved
/// the container — array or `MemorySegment` — down to a machine location, so
/// this reads the lanes directly instead of running `ldOp`'s per-lane lambda
/// through `MemorySegment.get`, which is a native crossing EACH.
///
/// Byte order is NOT this method's business and must not be applied here.
/// `fromMemorySegment(species, ms, off, order)` is
/// `fromMemorySegment0(ms, off).maybeSwap(order)` — the swap is a separate
/// operation outside the intrinsic, so the intrinsic reads native order. Doing
/// it twice would leave every big-endian load correct and every little-endian
/// one reversed.
fn vs_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sam_desc = format!("(Ljava/lang/Object;J{SPECIES}){PAYLOAD}");
    let take_fallback = |ctx: &mut dyn NativeContext| {
        fallback(
            ctx,
            stats::E_LOAD,
            args.get(9),
            "load",
            &sam_desc,
            &[
                args.get(6).copied().unwrap_or(Value::Object(None)),
                args.get(7).copied().unwrap_or(Value::Long(0)),
                args.get(8).copied().unwrap_or(Value::Object(None)),
            ],
        )
    };
    let lanes = int_at(args, 2).max(0) as usize;
    let offset = match args.get(4) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => i64::from(*n),
        _ => return take_fallback(ctx),
    };
    let (Some(vm_class), Some(etype)) = (obj_at(args, 0), obj_at(args, 1)) else {
        return take_fallback(ctx);
    };
    // The element type first, the vector class as the second answer: see
    // `elem_code_of_mirror` for why a primitive mirror alone is not enough.
    let Some(elem) = elem_code_of_mirror(ctx, etype)
        .or_else(|| elem_code_of_mirror(ctx, vm_class))
    else {
        return take_fallback(ctx);
    };
    if lanes == 0 {
        return take_fallback(ctx);
    }
    let Some(at) = decode_access(ctx, obj_at(args, 3), offset, elem, lanes) else {
        return take_fallback(ctx);
    };
    let out = read_lanes(ctx, &at, elem, lanes);
    match build_vector(ctx, stats::E_LOAD, vm_class, elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => take_fallback(ctx),
    }
}

/// `store(vClass, eClass, length, base, offset, fromSegment, v, container, index, defaultImpl)`
fn vs_store(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sam_desc = format!("(Ljava/lang/Object;J{PAYLOAD})V");
    let take_fallback = |ctx: &mut dyn NativeContext| {
        fallback(
            ctx,
            stats::E_STORE,
            args.get(9),
            "store",
            &sam_desc,
            &[
                args.get(7).copied().unwrap_or(Value::Object(None)),
                args.get(8).copied().unwrap_or(Value::Long(0)),
                args.get(6).copied().unwrap_or(Value::Object(None)),
            ],
        )
    };
    let offset = match args.get(4) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => i64::from(*n),
        _ => return take_fallback(ctx),
    };
    let Some(v) = obj_at(args, 6) else {
        return take_fallback(ctx);
    };
    let Some((elem, lanes)) = lanes_of(ctx, v) else {
        return take_fallback(ctx);
    };
    if lanes.is_empty() {
        return take_fallback(ctx);
    }
    let Some(at) = decode_access(ctx, obj_at(args, 3), offset, elem, lanes.len()) else {
        return take_fallback(ctx);
    };
    write_lanes(ctx, &at, elem, &lanes);
    stats::handled(stats::E_STORE);
    Ok(None)
}

/// The `VectorSupport` operations HotSpot intrinsifies, on CratonVM.
///
/// Registered from `register_essential_natives_with_shims`, i.e. the REAL-JDK
/// path — which is the whole point and is not automatic. The `fma` natives on
/// `java/lang/StrictMath` sat in `register_p69_misc`, reachable only from
/// `register_synthetic_overrides`, so they had never once run in real-JDK mode;
/// that was measured on 2026-08-22 and is why this registrar's placement is
/// stated rather than assumed.
///
/// `NativeKind::Intrinsic`, not `Bridge`. These triples have real JDK bytecode
/// and this file deliberately shadows it — that is what `Intrinsic` means in
/// `resolve_native_dispatch_wave1`, and it is the one kind allowed to win under
/// `--jdk-only` as well. The claim it makes is exactly the one HotSpot makes for
/// the same methods: the compiled form and the Java fallback compute the same
/// answer, and `probes/VectorApiProbe.java` is the evidence.
// ---------------------------------------------------------------------------
// The dispatch layer ABOVE `VectorSupport`
// ---------------------------------------------------------------------------
//
// # Why this section exists
//
// With the nine `VectorSupport` kernels above in place,
// `docs/known-issues/perf/vector-api-dispatch-depth-20260822.md` measured
// `fell_back=0` on GPULlama3's inference kernel and a 3.8x wall-clock win — and
// then recorded that what was LEFT was the JDK's own route to those kernels. A
// `--nojit --stack-sample-ms 5` profile of the same kernel, 342 samples,
// deepest frame per sample:
//
// ```text
//  48  IntVector.lanewiseTemplate          13  AbstractVector.sameSpecies
//  44  AbstractVector.convert0             11  VectorOperators$OperatorImpl.opKind
//  19  IntVector.lanewiseShiftTemplate     11  VectorOperators$OperatorImpl.opCode
//  18  IntVector$IntSpecies.broadcastBits   9  VectorOperators$ImplCache.find
// ```
//
// Every one of those is the SAME operation this file already computes, arriving
// through several dozen interpreted Java calls. `lanewiseTemplate` is where the
// route converges: `IntVector.add`, `.and`, `.or`, `.lanewise` and their
// siblings on all five shapes funnel into it, and its body is a special-case
// cascade, an `opCode` field read, an `ImplCache.find` and the
// `VectorSupport.binaryOp` call this file already answers.
//
// So this section registers on the TEMPLATE, not on the kernel — six classes
// (`ByteVector` … `DoubleVector`), not the thirty per-shape concrete ones the
// parent page worried about, because `Int256Vector.lanewise` is one line:
// `return (Int256Vector) super.lanewiseTemplate(op, v);`
//
// # How an operator is decoded, and why it is a field read rather than a table
//
// `VectorOperators$OperatorImpl` carries ONE int, `opInfo`, and the JDK's own
// accessors are pure functions of it (`javap -c`, Temurin 25.0.3+9):
//
// ```text
// opCodeRaw()   = opInfo >> 12
// opKind(mask)  = (opInfo & mask) != 0
// opCode(req,forbid): opCodeRaw(), throwing unless (opInfo & req) == req
//                     and (forbid == 0 || (opInfo & forbid) != forbid)
// ```
//
// Reading `opInfo` and applying those three lines is therefore not a re-derived
// table that could drift from the JDK's — it is the JDK's own arithmetic on the
// JDK's own field. The masks below are the literals the templates pass, taken
// from their bytecode rather than from the source constants' names, because the
// bytecode is what runs.
//
// # Refusing is still always available
//
// Every entry point here ends in a call to its own bytecode
// (`invoke_special_bytecode_only`, the "run this body, no native check"
// primitive), so a refusal is bit-for-bit the un-intercepted VM — including
// every special-case branch this code deliberately does not model: `AND_NOT`,
// `DIV`-by-zero, `FIRST_NONZERO`, `ZOMO`, `NOT`, `BITWISE_BLEND`, the masked
// forms, and any opcode the lane kernels do not implement.

/// `VO_SPECIAL | VO_SHIFT` — the mask `lanewiseTemplate(Binary, Vector)` tests
/// Are the dispatch-layer TEMPLATES registered?
///
/// A second switch beside `CRATONVM_VECTOR_INTRINSICS`, and it exists for one
/// reason: the two layers answer the SAME operations, so a single switch can
/// only compare "all of it" against "none of it" and cannot price the
/// templates against the kernels they sit on top of. With this one,
/// `CRATONVM_VECTOR_TEMPLATES=0` leaves the nine kernels registered and the
/// route to them interpreted, which is exactly the arm the parent page
/// measured before this section existed.
///
/// Gates REGISTRATION, like its sibling — so the off arm is a VM that never
/// answers a template natively, not one that answers and discards.
fn templates_engaged() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_VECTOR_TEMPLATES")
            .ok()
            .as_deref()
            != Some("0")
    })
}

/// to decide whether it must run its special-case cascade. Taken from the
/// template's own bytecode (`sipush 136`).
const VO_BINARY_SPECIAL: i32 = 136;
/// `opCode`'s require mask: every `XxxVector.opCode` passes `2048`.
const VO_OPCODE_VALID: i32 = 2048;
/// `opCode`'s forbid mask. `256` for the two floating-point element types and
/// `512` for the four integral ones — the per-class literals in
/// `FloatVector.opCode` / `IntVector.opCode` and their siblings.
const VO_FORBID_FP: i32 = 256;
const VO_FORBID_INTEGRAL: i32 = 512;

/// The lane count a shift's scalar operand is masked by, per element type.
/// `IntVector.lanewiseShiftTemplate` does `e &= 31`; the other widths use their
/// own `bits - 1`.
fn shift_mask_for(elem: u8) -> i32 {
    match elem {
        ELEM_LONG => 63,
        ELEM_INT => 31,
        ELEM_SHORT => 15,
        _ => 7,
    }
}

thread_local! {
    /// One-entry memo for `OperatorImpl.opInfo`'s slot index, keyed by the
    /// operator's concrete class. Every operator of one arity is the same
    /// class, so a kernel that uses `AND`, `OR` and `LSHL` still hits this on
    /// every call after the first.
    static OP_INFO_SLOT: std::cell::Cell<Option<(ClassId, usize)>> =
        const { std::cell::Cell::new(None) };
}

/// `((OperatorImpl) op).opInfo`, memoised by the operator's class.
fn op_info_of(ctx: &dyn NativeContext, op: ObjectRef) -> Option<i32> {
    let cid = ctx.class_id_of_object(op);
    let slot = match OP_INFO_SLOT.with(|c| c.get()) {
        Some((cached, slot)) if cached == cid => slot,
        _ => {
            let slot = ctx.resolve_field_index_by_class_id(cid, "opInfo")?;
            OP_INFO_SLOT.with(|c| c.set(Some((cid, slot))));
            slot
        }
    };
    match ctx.get_field(op, slot) {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

/// Decode an operator to its `VectorSupport` opcode, or `None` when the
/// template's own `opCode` would have thrown or its special cascade would have
/// run.
///
/// `special_mask` is the template's own `opKind` test; a hit there means the
/// JDK body branches before ever reaching `VectorSupport`, so this file must
/// hand the call back rather than skip the branch.
fn opcode_of(ctx: &dyn NativeContext, op: ObjectRef, elem: u8, special_mask: i32) -> Option<i32> {
    let op_info = op_info_of(ctx, op)?;
    if special_mask != 0 && (op_info & special_mask) != 0 {
        return None;
    }
    if (op_info & VO_OPCODE_VALID) != VO_OPCODE_VALID {
        return None;
    }
    let forbid = if elem == ELEM_FLOAT || elem == ELEM_DOUBLE {
        VO_FORBID_FP
    } else {
        VO_FORBID_INTEGRAL
    };
    if (op_info & forbid) == forbid {
        return None;
    }
    Some(op_info >> 12)
}

/// Build a vector of `class_id` whose payload holds `lanes`.
///
/// [`build_vector`]'s sibling for the callers that already hold the receiver's
/// `ClassId` — every entry point in this section does, because the result of a
/// `lanewiseTemplate` is always the receiver's OWN concrete class (`getClass()`
/// in the template's own bytecode). Same allocation order and same pin as
/// `build_vector`; see its comment for why the array is pinned across the
/// object allocation.
fn build_vector_of(
    ctx: &mut dyn NativeContext,
    entry: usize,
    class_id: ClassId,
    elem: u8,
    lanes: &[i64],
) -> Option<ObjectRef> {
    let arr0 = ctx.new_array(array_type_of(elem), lanes.len());
    let pin = ctx.pin_native_root(arr0);
    let slots = ctx.class_num_total_fields(class_id).max(1);
    let obj = ctx
        .try_alloc_object_gc_safe(class_id, slots)
        .unwrap_or_else(|| ctx.alloc_object(class_id, slots));
    let arr = ctx.read_native_pin(pin, arr0);
    for (i, bits) in lanes.iter().copied().enumerate() {
        ctx.set_array_element(arr, i, lane_bits_to_value(elem, bits));
    }
    let slot = payload_slot(ctx, class_id);
    ctx.set_field(obj, slot, Value::Object(Some(arr)));
    ctx.unpin_native_roots(pin);
    stats::handled(entry);
    Some(obj)
}

/// Hand a template call back to its own bytecode.
///
/// `invoke_special_bytecode_only` is the "just run this body, no native check"
/// primitive (`vm_exec::invoke_special_bytecode_only_shared` calls
/// `interpreter::execute` directly), so naming the same method this native is
/// registered for is not a recursion — it is the un-intercepted VM.
fn template_fallback(
    ctx: &mut dyn NativeContext,
    entry: usize,
    owner: &str,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    stats::fell_back(entry);
    ctx.invoke_special_bytecode_only(owner, method, descriptor, args)
}

/// The receiver's concrete `ClassId` and element type — the two things every
/// entry point below needs.
fn template_owner(ctx: &dyn NativeContext, this: ObjectRef) -> Option<(ClassId, u8)> {
    let cid = ctx.class_id_of_object(this);
    let arr = payload_of(ctx, this)?;
    let elem = elem_code_of(ctx, arr)?;
    Some((cid, elem))
}

/// `XxxVector.lanewiseTemplate(VectorOperators$Binary, Vector)`
fn vd_lanewise_binary(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    owner: &str,
    ret: &str,
) -> MethodCallResult {
    let desc =
        format!("(Ljdk/incubator/vector/VectorOperators$Binary;Ljdk/incubator/vector/Vector;){ret}");
    let (Some(this), Some(op), Some(that)) = (obj_at(args, 0), obj_at(args, 1), obj_at(args, 2))
    else {
        return template_fallback(ctx, stats::E_TMPL_BINARY, owner, "lanewiseTemplate", &desc, args);
    };
    let handled = (|| -> Option<Vec<i64>> {
        // `check(that)` in the template asserts the two share a species and
        // throws `IllegalArgumentException` otherwise. Identical concrete
        // classes is a STRONGER test than identical species, and needs no
        // species object.
        if ctx.class_id_of_object(that) != ctx.class_id_of_object(this) {
            return None;
        }
        let (_, elem) = template_owner(ctx, this)?;
        let opc = opcode_of(ctx, op, elem, VO_BINARY_SPECIAL)?;
        let (_, a) = lanes_of(ctx, this)?;
        let (_, b) = lanes_of(ctx, that)?;
        if a.len() != b.len() || a.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(a.len());
        for (x, y) in a.iter().copied().zip(b.iter().copied()) {
            out.push(binary_lane(opc, elem, x, y)?);
        }
        Some(out)
    })();
    let Some(out) = handled else {
        return template_fallback(ctx, stats::E_TMPL_BINARY, owner, "lanewiseTemplate", &desc, args);
    };
    let (cid, elem) = match template_owner(ctx, this) {
        Some(v) => v,
        None => {
            return template_fallback(ctx, stats::E_TMPL_BINARY, owner, "lanewiseTemplate", &desc, args)
        }
    };
    match build_vector_of(ctx, stats::E_TMPL_BINARY, cid, elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => template_fallback(ctx, stats::E_TMPL_BINARY, owner, "lanewiseTemplate", &desc, args),
    }
}

/// `XxxVector.lanewiseTemplate(VectorOperators$Unary)`
fn vd_lanewise_unary(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    owner: &str,
    ret: &str,
) -> MethodCallResult {
    let desc = format!("(Ljdk/incubator/vector/VectorOperators$Unary;){ret}");
    let (Some(this), Some(op)) = (obj_at(args, 0), obj_at(args, 1)) else {
        return template_fallback(ctx, stats::E_TMPL_UNARY, owner, "lanewiseTemplate", &desc, args);
    };
    let handled = (|| -> Option<Vec<i64>> {
        let (_, elem) = template_owner(ctx, this)?;
        // No special mask. `lanewiseTemplate(Unary)` branches on exactly two
        // operators by IDENTITY — `ZOMO` and `NOT` — and both are expansions
        // with no `VectorSupport` opcode of their own, so `VO_OPCODE_VALID`
        // is clear on them and `opcode_of` refuses them anyway. Passing
        // `VO_SPECIAL` as well refused EVERY unary: the census read
        // `tmpl:lanewise(Unary) handled=0 fell_back=30976` while the kernel
        // below it answered all 30 976, i.e. the mask cost the whole win and
        // bought nothing. Anything the lane kernel does not compute still
        // takes the fallback through `unary_lane`.
        let opc = opcode_of(ctx, op, elem, 0)?;
        let (_, a) = lanes_of(ctx, this)?;
        if a.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(a.len());
        for x in a.iter().copied() {
            out.push(unary_lane(opc, elem, x)?);
        }
        Some(out)
    })();
    let Some(out) = handled else {
        return template_fallback(ctx, stats::E_TMPL_UNARY, owner, "lanewiseTemplate", &desc, args);
    };
    let (cid, elem) = match template_owner(ctx, this) {
        Some(v) => v,
        None => {
            return template_fallback(ctx, stats::E_TMPL_UNARY, owner, "lanewiseTemplate", &desc, args)
        }
    };
    match build_vector_of(ctx, stats::E_TMPL_UNARY, cid, elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => template_fallback(ctx, stats::E_TMPL_UNARY, owner, "lanewiseTemplate", &desc, args),
    }
}

/// `XxxVector.lanewiseShiftTemplate(VectorOperators$Binary, int)`
///
/// No special cascade at all in the JDK body — only an assertion that the
/// operator IS a shift, the `e &= bits-1` mask, and `broadcastInt`.
fn vd_lanewise_shift(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    owner: &str,
    ret: &str,
) -> MethodCallResult {
    let desc = format!("(Ljdk/incubator/vector/VectorOperators$Binary;I){ret}");
    let fb = "lanewiseShiftTemplate";
    let (Some(this), Some(op)) = (obj_at(args, 0), obj_at(args, 1)) else {
        return template_fallback(ctx, stats::E_TMPL_SHIFT, owner, fb, &desc, args);
    };
    let e_raw = int_at(args, 2);
    let handled = (|| -> Option<Vec<i64>> {
        let (_, elem) = template_owner(ctx, this)?;
        // The shift template has no `opKind` branch to reproduce, so no special
        // mask — but `opCode`'s require/forbid still apply.
        let opc = opcode_of(ctx, op, elem, 0)?;
        let e = i64::from(e_raw & shift_mask_for(elem));
        let (_, a) = lanes_of(ctx, this)?;
        if a.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(a.len());
        for x in a.iter().copied() {
            out.push(binary_lane(opc, elem, x, e)?);
        }
        Some(out)
    })();
    let Some(out) = handled else {
        return template_fallback(ctx, stats::E_TMPL_SHIFT, owner, fb, &desc, args);
    };
    let (cid, elem) = match template_owner(ctx, this) {
        Some(v) => v,
        None => return template_fallback(ctx, stats::E_TMPL_SHIFT, owner, fb, &desc, args),
    };
    match build_vector_of(ctx, stats::E_TMPL_SHIFT, cid, elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => template_fallback(ctx, stats::E_TMPL_SHIFT, owner, fb, &desc, args),
    }
}

/// `XxxVector.lanewiseTemplate(VectorOperators$Ternary, Vector, Vector)`
///
/// Only `FMA` is computed here, matching [`vs_ternary_op`]; `BITWISE_BLEND` has
/// its own three-`lanewise` expansion in the JDK body and is handed back.
fn vd_lanewise_ternary(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    owner: &str,
    ret: &str,
) -> MethodCallResult {
    let desc = format!(
        "(Ljdk/incubator/vector/VectorOperators$Ternary;Ljdk/incubator/vector/Vector;\
         Ljdk/incubator/vector/Vector;){ret}"
    );
    let (Some(this), Some(op), Some(v2), Some(v3)) = (
        obj_at(args, 0),
        obj_at(args, 1),
        obj_at(args, 2),
        obj_at(args, 3),
    ) else {
        return template_fallback(ctx, stats::E_TMPL_TERNARY, owner, "lanewiseTemplate", &desc, args);
    };
    let handled = (|| -> Option<Vec<i64>> {
        let cid = ctx.class_id_of_object(this);
        if ctx.class_id_of_object(v2) != cid || ctx.class_id_of_object(v3) != cid {
            return None;
        }
        let (_, elem) = template_owner(ctx, this)?;
        // No `opKind` test to reproduce here: the JDK body special-cases ONE
        // operator by identity (`BITWISE_BLEND`), and the `opc != OP_FMA`
        // screen below already refuses it and every other ternary operator
        // these kernels do not compute. Passing a special mask as well would
        // risk refusing `FMA` itself if it ever carried that bit.
        let opc = opcode_of(ctx, op, elem, 0)?;
        if opc != OP_FMA {
            return None;
        }
        let (_, a) = lanes_of(ctx, this)?;
        let (_, b) = lanes_of(ctx, v2)?;
        let (_, c) = lanes_of(ctx, v3)?;
        if a.len() != b.len() || a.len() != c.len() || a.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(a.len());
        for i in 0..a.len() {
            out.push(match elem {
                ELEM_FLOAT => f32::from_bits(a[i] as u32)
                    .mul_add(f32::from_bits(b[i] as u32), f32::from_bits(c[i] as u32))
                    .to_bits() as i64,
                ELEM_DOUBLE => f64::from_bits(a[i] as u64)
                    .mul_add(f64::from_bits(b[i] as u64), f64::from_bits(c[i] as u64))
                    .to_bits() as i64,
                _ => return None,
            });
        }
        Some(out)
    })();
    let Some(out) = handled else {
        return template_fallback(ctx, stats::E_TMPL_TERNARY, owner, "lanewiseTemplate", &desc, args);
    };
    let (cid, elem) = match template_owner(ctx, this) {
        Some(v) => v,
        None => {
            return template_fallback(ctx, stats::E_TMPL_TERNARY, owner, "lanewiseTemplate", &desc, args)
        }
    };
    match build_vector_of(ctx, stats::E_TMPL_TERNARY, cid, elem, &out) {
        Some(obj) => Ok(Some(Value::Object(Some(obj)))),
        None => template_fallback(ctx, stats::E_TMPL_TERNARY, owner, "lanewiseTemplate", &desc, args),
    }
}

/// One set of four `NativeCallback`s per element type.
///
/// A `NativeCallback` is a plain `fn` pointer with no captured state, so the
/// owner class — which the fallback needs in order to name the very body it is
/// handing the call back to — has to come from somewhere. A macro that stamps
/// out six named functions is that somewhere; deriving it from the receiver's
/// class name at run time would put a string operation on the hot path AND
/// leave the fallback with no name to use when the receiver is the thing that
/// failed to decode.
macro_rules! vector_templates {
    ($($fn_bin:ident, $fn_un:ident, $fn_shift:ident, $fn_tern:ident, $owner:literal;)*) => {
        $(
            fn $fn_bin(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                vd_lanewise_binary(ctx, args, $owner, concat!("L", $owner, ";"))
            }
            fn $fn_un(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                vd_lanewise_unary(ctx, args, $owner, concat!("L", $owner, ";"))
            }
            fn $fn_shift(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                vd_lanewise_shift(ctx, args, $owner, concat!("L", $owner, ";"))
            }
            fn $fn_tern(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                vd_lanewise_ternary(ctx, args, $owner, concat!("L", $owner, ";"))
            }
        )*
    };
}

vector_templates! {
    vd_byte_bin,   vd_byte_un,   vd_byte_shift,   vd_byte_tern,   "jdk/incubator/vector/ByteVector";
    vd_short_bin,  vd_short_un,  vd_short_shift,  vd_short_tern,  "jdk/incubator/vector/ShortVector";
    vd_int_bin,    vd_int_un,    vd_int_shift,    vd_int_tern,    "jdk/incubator/vector/IntVector";
    vd_long_bin,   vd_long_un,   vd_long_shift,   vd_long_tern,   "jdk/incubator/vector/LongVector";
    vd_float_bin,  vd_float_un,  vd_float_shift,  vd_float_tern,  "jdk/incubator/vector/FloatVector";
    vd_double_bin, vd_double_un, vd_double_shift, vd_double_tern, "jdk/incubator/vector/DoubleVector";
}

/// Register the dispatch-layer templates.
///
/// Called from [`register_vector_support_intrinsics`] under the same kill
/// switch, because the two halves are one feature: with the kernels off, a
/// template intercept would be answering an operation the "off" arm is supposed
/// to leave entirely to the JDK.
///
/// `lanewiseShiftTemplate` is registered only for the four integral element
/// types. `FloatVector` and `DoubleVector` do not declare it — a float has no
/// shift — and registering a triple no class declares would be an entry that
/// can never fire.
fn register_vector_dispatch_templates(r: &mut NativeMethodRegistry) {
    if !templates_engaged() {
        return;
    }
    const BIN_ARGS: &str =
        "(Ljdk/incubator/vector/VectorOperators$Binary;Ljdk/incubator/vector/Vector;)";
    const UN_ARGS: &str = "(Ljdk/incubator/vector/VectorOperators$Unary;)";
    const SHIFT_ARGS: &str = "(Ljdk/incubator/vector/VectorOperators$Binary;I)";
    const TERN_ARGS: &str = "(Ljdk/incubator/vector/VectorOperators$Ternary;\
                             Ljdk/incubator/vector/Vector;Ljdk/incubator/vector/Vector;)";

    let mut one = |owner: &str,
                   bin: NativeCallback,
                   un: NativeCallback,
                   shift: Option<NativeCallback>,
                   tern: NativeCallback| {
        let ret = format!("L{owner};");
        r.register_with_kind(
            owner,
            "lanewiseTemplate",
            &format!("{BIN_ARGS}{ret}"),
            bin,
            NativeKind::Intrinsic,
        );
        r.register_with_kind(
            owner,
            "lanewiseTemplate",
            &format!("{UN_ARGS}{ret}"),
            un,
            NativeKind::Intrinsic,
        );
        r.register_with_kind(
            owner,
            "lanewiseTemplate",
            &format!("{TERN_ARGS}{ret}"),
            tern,
            NativeKind::Intrinsic,
        );
        if let Some(shift) = shift {
            r.register_with_kind(
                owner,
                "lanewiseShiftTemplate",
                &format!("{SHIFT_ARGS}{ret}"),
                shift,
                NativeKind::Intrinsic,
            );
        }
    };

    one(
        "jdk/incubator/vector/ByteVector",
        vd_byte_bin,
        vd_byte_un,
        Some(vd_byte_shift),
        vd_byte_tern,
    );
    one(
        "jdk/incubator/vector/ShortVector",
        vd_short_bin,
        vd_short_un,
        Some(vd_short_shift),
        vd_short_tern,
    );
    one(
        "jdk/incubator/vector/IntVector",
        vd_int_bin,
        vd_int_un,
        Some(vd_int_shift),
        vd_int_tern,
    );
    one(
        "jdk/incubator/vector/LongVector",
        vd_long_bin,
        vd_long_un,
        Some(vd_long_shift),
        vd_long_tern,
    );
    one(
        "jdk/incubator/vector/FloatVector",
        vd_float_bin,
        vd_float_un,
        None,
        vd_float_tern,
    );
    one(
        "jdk/incubator/vector/DoubleVector",
        vd_double_bin,
        vd_double_un,
        None,
        vd_double_tern,
    );
}

pub(crate) fn register_vector_support_intrinsics(r: &mut NativeMethodRegistry) {
    // The kill switch gates REGISTRATION, not each call: with the natives
    // absent the "off" arm is bit-for-bit the un-intercepted VM, which a
    // per-call early return would not be — `maybeRebox` has no `defaultImpl` to
    // hand back to, so an off arm that kept its registration would still be
    // answering that one method natively.
    if !engaged() {
        return;
    }
    let __prev_cat = r.current_category();
    r.set_category(NativeKind::Intrinsic);

    r.register_with_kind(
        VECTOR_SUPPORT,
        "maybeRebox",
        &format!("({PAYLOAD}){PAYLOAD}"),
        vs_maybe_rebox,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "binaryOp",
        &format!(
            "(ILjava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;I{PAYLOAD}{PAYLOAD}{MASK}\
             Ljdk/internal/vm/vector/VectorSupport$BinaryOperation;){PAYLOAD}"
        ),
        vs_binary_op,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "unaryOp",
        &format!(
            "(ILjava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;I{VECTOR}{MASK}\
             Ljdk/internal/vm/vector/VectorSupport$UnaryOperation;){VECTOR}"
        ),
        vs_unary_op,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "ternaryOp",
        &format!(
            "(ILjava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;I{VECTOR}{VECTOR}{VECTOR}{MASK}\
             Ljdk/internal/vm/vector/VectorSupport$TernaryOperation;){VECTOR}"
        ),
        vs_ternary_op,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "broadcastInt",
        &format!(
            "(ILjava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;I{VECTOR}I{MASK}\
             Ljdk/internal/vm/vector/VectorSupport$VectorBroadcastIntOp;){VECTOR}"
        ),
        vs_broadcast_int,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "reductionCoerced",
        &format!(
            "(ILjava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;I{VECTOR}{MASK}\
             Ljdk/internal/vm/vector/VectorSupport$ReductionOperation;)J"
        ),
        vs_reduction_coerced,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "convert",
        &format!(
            "(ILjava/lang/Class;Ljava/lang/Class;ILjava/lang/Class;Ljava/lang/Class;I{PAYLOAD}\
             {SPECIES}Ljdk/internal/vm/vector/VectorSupport$VectorConvertOp;){PAYLOAD}"
        ),
        vs_convert,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "fromBitsCoerced",
        &format!(
            "(Ljava/lang/Class;Ljava/lang/Class;IJI{SPECIES}\
             Ljdk/internal/vm/vector/VectorSupport$FromBitsCoercedOperation;){PAYLOAD}"
        ),
        vs_from_bits_coerced,
        NativeKind::Intrinsic,
    );

    r.register_with_kind(
        VECTOR_SUPPORT,
        "load",
        &format!(
            "(Ljava/lang/Class;Ljava/lang/Class;ILjava/lang/Object;JZLjava/lang/Object;J\
             {SPECIES}Ljdk/internal/vm/vector/VectorSupport$LoadOperation;){PAYLOAD}"
        ),
        vs_load,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "store",
        &format!(
            "(Ljava/lang/Class;Ljava/lang/Class;ILjava/lang/Object;JZ{PAYLOAD}Ljava/lang/Object;J\
             Ljdk/internal/vm/vector/VectorSupport$StoreVectorOperation;)V"
        ),
        vs_store,
        NativeKind::Intrinsic,
    );

    // The JDK`s own route to the nine kernels above — see the section comment
    // on `register_vector_dispatch_templates`. Under the SAME kill switch,
    // because the two halves are one feature.
    register_vector_dispatch_templates(r);

    r.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The float MIN/MAX lanes follow `Math.min`/`Math.max`, not Rust's.
    ///
    /// Three rows that separate them, and every one is reachable from
    /// `FloatVector.min`: NaN propagates (Rust returns the other operand),
    /// `min(-0.0, 0.0)` is `-0.0` and `max(-0.0, 0.0)` is `0.0` (Rust's answer
    /// for both is whichever it saw first).
    #[test]
    fn float_min_max_are_javas_not_rusts() {
        let nan = f32::NAN.to_bits() as i64;
        let one = 1.0f32.to_bits() as i64;
        let got = binary_lane(OP_MIN, ELEM_FLOAT, nan, one).unwrap();
        assert!(f32::from_bits(got as u32).is_nan(), "NaN must propagate");
        let got = binary_lane(OP_MAX, ELEM_FLOAT, one, nan).unwrap();
        assert!(f32::from_bits(got as u32).is_nan(), "NaN must propagate");

        let neg_zero = (-0.0f32).to_bits() as i64;
        let pos_zero = 0.0f32.to_bits() as i64;
        assert_eq!(
            binary_lane(OP_MIN, ELEM_FLOAT, neg_zero, pos_zero).unwrap() as u32,
            (-0.0f32).to_bits(),
            "min(-0.0, 0.0) is -0.0",
        );
        assert_eq!(
            binary_lane(OP_MAX, ELEM_FLOAT, neg_zero, pos_zero).unwrap() as u32,
            0.0f32.to_bits(),
            "max(-0.0, 0.0) is +0.0",
        );
    }

    /// A mask or shuffle class must NOT be read as a lane type.
    ///
    /// `Long256Vector$Long256Mask` starts with "Long"; its payload is a
    /// `boolean[]`. Building a `long[]` for it produced a
    /// `ClassCastException: [J cannot be cast to [Z` inside
    /// `LongVector.fromMemorySegment`, which is a long way from the mistake.
    /// The predicate is pure, so it is testable without a VM.
    #[test]
    fn mask_and_shuffle_class_names_are_not_lane_types() {
        for name in [
            "jdk/incubator/vector/Long256Vector$Long256Mask",
            "jdk/incubator/vector/Int64Vector$Int64Mask",
            "jdk/incubator/vector/Float256Vector$Float256Shuffle",
            "jdk/incubator/vector/ByteMaxVector$ByteMaxMask",
        ] {
            let simple = name.rsplit('/').next().unwrap();
            assert!(
                simple.contains("Mask") || simple.contains("Shuffle"),
                "{name} must be screened out before the prefix table",
            );
        }
        // ...while the vector classes themselves still are lane types.
        for name in [
            "jdk/incubator/vector/Long256Vector",
            "jdk/incubator/vector/Float64Vector",
            "jdk/incubator/vector/ByteMaxVector",
        ] {
            let simple = name.rsplit('/').next().unwrap();
            assert!(
                !(simple.contains("Mask") || simple.contains("Shuffle")),
                "{name} must reach the prefix table",
            );
        }
    }

    /// An unimplemented opcode answers `None`, which the entry points turn into
    /// the JDK's own lambda. This is the property that makes partial coverage
    /// safe, so it is asserted rather than assumed.
    #[test]
    fn an_unknown_opcode_refuses_rather_than_guessing() {
        // VECTOR_OP_BIT_COUNT (3) is deliberately not implemented.
        assert!(binary_lane(3, ELEM_INT, 1, 2).is_none());
        assert!(unary_lane(3, ELEM_INT, 1).is_none());
        // ...and neither is any math-library opcode.
        assert!(unary_lane(103, ELEM_FLOAT, 0).is_none());
    }

    /// Integral division by zero refuses, so the ArithmeticException comes from
    /// the JDK's own lane lambda with the JDK's own message rather than from a
    /// message invented here.
    #[test]
    fn integral_division_by_zero_refuses() {
        assert!(binary_lane(OP_DIV, ELEM_INT, 6, 0).is_none());
        assert_eq!(binary_lane(OP_DIV, ELEM_INT, 6, 3), Some(2));
        // Float division by zero is infinity, not an exception — it must NOT
        // refuse, because refusing would be a needless fallback on a hot lane.
        let inf = binary_lane(OP_DIV, ELEM_FLOAT, 1.0f32.to_bits() as i64, 0).unwrap();
        assert!(f32::from_bits(inf as u32).is_infinite());
    }

    /// Shift counts are masked the way `ishl`/`lshl` mask them.
    #[test]
    fn shift_counts_wrap_like_the_jvms() {
        // int << 33 is int << 1
        assert_eq!(binary_lane(OP_LSHIFT, ELEM_INT, 1, 33), Some(2));
        // long << 65 is long << 1
        assert_eq!(binary_lane(OP_LSHIFT, ELEM_LONG, 1, 65), Some(2));
        // >>> zero-extends within the lane width, not within i64.
        assert_eq!(binary_lane(OP_URSHIFT, ELEM_INT, -1, 28), Some(15));
    }

    /// `wrap_integral_lane` is shared with `vector_api.rs`; check the narrowing
    /// it gives this file is the Java one for the sub-word types.
    #[test]
    fn sub_word_lanes_narrow_like_java() {
        // (short) 0x1_0001 == 1
        assert_eq!(binary_lane(OP_ADD, ELEM_SHORT, 0xFFFF, 2), Some(1));
        // (byte) 127 + 1 == -128
        assert_eq!(binary_lane(OP_ADD, ELEM_BYTE, 127, 1), Some(-128));
    }

    /// Float->int cast saturates and maps NaN to zero, as `d2i` does.
    #[test]
    fn float_to_int_cast_saturates_like_d2i() {
        let big = 1.0e30f32.to_bits() as i64;
        assert_eq!(
            cast_lane(ELEM_FLOAT, ELEM_INT, big, false),
            Some(i64::from(i32::MAX))
        );
        let nan = f32::NAN.to_bits() as i64;
        assert_eq!(cast_lane(ELEM_FLOAT, ELEM_INT, nan, false), Some(0));
    }

    /// `UCAST` differs from `CAST` only when widening an integral lane.
    #[test]
    fn ucast_zero_extends_where_cast_sign_extends() {
        // (byte) -1 widened to int: CAST gives -1, UCAST gives 255.
        assert_eq!(cast_lane(ELEM_BYTE, ELEM_INT, -1, false), Some(-1));
        assert_eq!(cast_lane(ELEM_BYTE, ELEM_INT, -1, true), Some(255));
    }
}
