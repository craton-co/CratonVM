// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! SIMD Vector API native method implementations (JEP 508 — 10th Incubator, JDK 25).
//!
//! Covers:
//!   - VectorSpecies — species constants and configuration
//!   - IntVector, LongVector, FloatVector, DoubleVector — lane-wise operations
//!   - VectorMask — boolean lane masks
//!   - VectorShuffle — lane reordering
//!   - VectorOperators — operation code constants

use crate::{try_alloc_concurrent_synthetic, obj_arg};
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use cratonvm_types::error::MethodCallFailed;

// ---------------------------------------------------------------------------
// Element type codes
// ---------------------------------------------------------------------------
pub(crate) const ELEM_BYTE: u8 = 0;
pub(crate) const ELEM_SHORT: u8 = 1;
pub(crate) const ELEM_INT: u8 = 2;
pub(crate) const ELEM_LONG: u8 = 3;
pub(crate) const ELEM_FLOAT: u8 = 4;
pub(crate) const ELEM_DOUBLE: u8 = 5;

// ---------------------------------------------------------------------------
// VectorSpeciesConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VectorSpeciesConfig {
    pub element_type: u8,
    pub bit_size: u16,
    pub lane_count: u16,
}

impl VectorSpeciesConfig {
    pub fn new(element_type: u8, bit_size: u16) -> Self {
        let elem_bits = element_size_bits(element_type);
        let lane_count = if elem_bits > 0 {
            bit_size / elem_bits
        } else {
            0
        };
        Self {
            element_type,
            bit_size,
            lane_count,
        }
    }
}

fn element_size_bits(elem_type: u8) -> u16 {
    match elem_type {
        ELEM_BYTE => 8,
        ELEM_SHORT => 16,
        ELEM_INT => 32,
        ELEM_LONG => 64,
        ELEM_FLOAT => 32,
        ELEM_DOUBLE => 64,
        _ => 0,
    }
}

// Predefined species: (element_type, bit_size)
const SPECIES_CONFIGS: &[(u8, u16)] = &[
    (ELEM_INT, 64),     // 0: SPECIES_64 Int
    (ELEM_INT, 128),    // 1: SPECIES_128 Int
    (ELEM_INT, 256),    // 2: SPECIES_256 Int (preferred)
    (ELEM_INT, 512),    // 3: SPECIES_512 Int
    (ELEM_LONG, 64),    // 4: SPECIES_64 Long
    (ELEM_LONG, 128),   // 5: SPECIES_128 Long
    (ELEM_LONG, 256),   // 6: SPECIES_256 Long
    (ELEM_LONG, 512),   // 7: SPECIES_512 Long
    (ELEM_FLOAT, 64),   // 8: SPECIES_64 Float
    (ELEM_FLOAT, 128),  // 9: SPECIES_128 Float
    (ELEM_FLOAT, 256),  // 10: SPECIES_256 Float
    (ELEM_FLOAT, 512),  // 11: SPECIES_512 Float
    (ELEM_DOUBLE, 64),  // 12: SPECIES_64 Double
    (ELEM_DOUBLE, 128), // 13: SPECIES_128 Double
    (ELEM_DOUBLE, 256), // 14: SPECIES_256 Double
    (ELEM_DOUBLE, 512), // 15: SPECIES_512 Double
    (ELEM_BYTE, 64),    // 16: SPECIES_64 Byte
    (ELEM_BYTE, 128),   // 17: SPECIES_128 Byte
    (ELEM_BYTE, 256),   // 18: SPECIES_256 Byte
    (ELEM_BYTE, 512),   // 19: SPECIES_512 Byte
    (ELEM_SHORT, 64),   // 20: SPECIES_64 Short
    (ELEM_SHORT, 128),  // 21: SPECIES_128 Short
    (ELEM_SHORT, 256),  // 22: SPECIES_256 Short
    (ELEM_SHORT, 512),  // 23: SPECIES_512 Short
];

fn get_species_config(idx: usize) -> VectorSpeciesConfig {
    if idx < SPECIES_CONFIGS.len() {
        let (et, bs) = SPECIES_CONFIGS[idx];
        VectorSpeciesConfig::new(et, bs)
    } else {
        VectorSpeciesConfig::new(ELEM_INT, 256) // fallback: preferred
    }
}

// ---------------------------------------------------------------------------
// VectorOperators op codes
// ---------------------------------------------------------------------------
// Unary
const OP_NEG: i32 = 0;
const OP_ABS: i32 = 1;
const OP_NOT: i32 = 2;
const OP_SQRT: i32 = 3;
// Binary
const OP_ADD: i32 = 4;
const OP_SUB: i32 = 5;
const OP_MUL: i32 = 6;
const OP_DIV: i32 = 7;
const OP_AND: i32 = 8;
const OP_OR: i32 = 9;
const OP_XOR: i32 = 10;
const OP_MIN: i32 = 11;
const OP_MAX: i32 = 12;
// Ternary
const OP_FMA: i32 = 20;
// Comparison
const CMP_EQ: i32 = 30;
const CMP_NE: i32 = 31;
const CMP_LT: i32 = 32;
const CMP_LE: i32 = 33;
const CMP_GT: i32 = 34;
const CMP_GE: i32 = 35;
// Reduction codes (different namespace from binary ops)
const RED_ADD: i32 = 0;
const RED_MUL: i32 = 1;
const RED_MIN: i32 = 2;
const RED_MAX: i32 = 3;
const RED_AND: i32 = 4;
const RED_OR: i32 = 5;
const RED_XOR: i32 = 6;

fn op_name(code: i32) -> &'static str {
    match code {
        OP_NEG => "NEG",
        OP_ABS => "ABS",
        OP_NOT => "NOT",
        OP_SQRT => "SQRT",
        OP_ADD => "ADD",
        OP_SUB => "SUB",
        OP_MUL => "MUL",
        OP_DIV => "DIV",
        OP_AND => "AND",
        OP_OR => "OR",
        OP_XOR => "XOR",
        OP_MIN => "MIN",
        OP_MAX => "MAX",
        OP_FMA => "FMA",
        CMP_EQ => "EQ",
        CMP_NE => "NE",
        CMP_LT => "LT",
        CMP_LE => "LE",
        CMP_GT => "GT",
        CMP_GE => "GE",
        _ => "UNKNOWN",
    }
}

fn is_associative(code: i32) -> bool {
    matches!(
        code,
        OP_ADD | OP_MUL | OP_AND | OP_OR | OP_XOR | OP_MIN | OP_MAX
    )
}

// ---------------------------------------------------------------------------
// Helper: allocate a vector synthetic object
// ---------------------------------------------------------------------------
// IntVector/LongVector/FloatVector/DoubleVector layout:
//   field 0: species_idx (Int)
//   field 1: lane_count  (Int)
//   field 2: lane payload (long[]; integer lanes sign-extended, float lanes raw bits)
//   field 3: op_count    (Int)

fn alloc_vector(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    species_idx: i32,
    lane_count: i32,
    data_hash: i32,
    op_count: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let lanes = vec![data_hash as i64; normalized_lane_count(lane_count)];
    Ok(alloc_vector_lanes(ctx, class_name, species_idx, lane_count, &lanes, op_count)?)
}

fn alloc_vector_lanes(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    species_idx: i32,
    lane_count: i32,
    lanes: &[i64],
    op_count: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, class_name, 4)?;
    let lane_count_usize = normalized_lane_count(lane_count);
    let payload = ctx.new_array(ArrayElementType::Long, lane_count_usize);
    for i in 0..lane_count_usize {
        let lane = lanes.get(i).copied().unwrap_or(0);
        ctx.set_array_element(payload, i, Value::Long(lane));
    }
    ctx.set_field(obj, 0, Value::Int(species_idx));
    ctx.set_field(obj, 1, Value::Int(lane_count));
    ctx.set_field(obj, 2, Value::Object(Some(payload)));
    ctx.set_field(obj, 3, Value::Int(op_count));
    Ok(obj)
}

fn normalized_lane_count(lane_count: i32) -> usize {
    usize::try_from(lane_count)
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(1)
}

fn summarize_lanes(lanes: &[i64]) -> i32 {
    lanes.iter().fold(0i32, |acc, lane| {
        acc.wrapping_mul(31).wrapping_add(*lane as i32)
    })
}

fn read_vector_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) -> (i32, i32, i32, i32) {
    let (species_idx, lane_count, lanes, op_count) = read_vector_lanes(ctx, obj);
    (species_idx, lane_count, summarize_lanes(&lanes), op_count)
}

fn read_vector_lanes(ctx: &mut dyn NativeContext, obj: ObjectRef) -> (i32, i32, Vec<i64>, i32) {
    let species_idx = match ctx.get_field(obj, 0) {
        Value::Int(n) => n,
        _ => 0,
    };
    let lane_count = match ctx.get_field(obj, 1) {
        Value::Int(n) => n,
        _ => 0,
    };
    let op_count = match ctx.get_field(obj, 3) {
        Value::Int(n) => n,
        _ => 0,
    };
    let lane_count_usize = normalized_lane_count(lane_count);
    let lanes = match ctx.get_field(obj, 2) {
        Value::Object(Some(payload)) => {
            let payload_len = ctx.array_length(payload);
            let mut out = Vec::with_capacity(lane_count_usize);
            for i in 0..lane_count_usize {
                let lane = if i < payload_len {
                    match ctx.get_array_element(payload, i) {
                        Value::Long(n) => n,
                        Value::Int(n) => n as i64,
                        _ => 0,
                    }
                } else {
                    0
                };
                out.push(lane);
            }
            out
        }
        Value::Int(n) => vec![n as i64; lane_count_usize],
        _ => vec![0; lane_count_usize],
    };
    (species_idx, lane_count, lanes, op_count)
}

fn species_idx_from_arg(ctx: &mut dyn NativeContext, args: &[Value]) -> i32 {
    match args.first() {
        Some(Value::Object(Some(o))) => match ctx.get_field(*o, 0) {
            Value::Int(n) => n,
            _ => 2, // default preferred
        },
        _ => 2,
    }
}

fn lane_count_from_species(species_idx: i32) -> i32 {
    let cfg = get_species_config(species_idx as usize);
    cfg.lane_count as i32
}

fn int_arg(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(n)) => *n,
        Some(Value::Long(n)) => *n as i32,
        Some(Value::Float(f)) => *f as i32,
        Some(Value::Double(f)) => *f as i32,
        _ => 0,
    }
}

fn lane_arg_bits(args: &[Value], idx: usize, elem_type: u8) -> i64 {
    match elem_type {
        ELEM_BYTE => int_arg(args, idx) as i8 as i64,
        ELEM_SHORT => int_arg(args, idx) as i16 as i64,
        ELEM_INT => int_arg(args, idx) as i64,
        ELEM_LONG => match args.get(idx) {
            Some(Value::Long(n)) => *n,
            Some(Value::Int(n)) => *n as i64,
            _ => 0,
        },
        ELEM_FLOAT => match args.get(idx) {
            Some(Value::Float(f)) => f.to_bits() as i64,
            Some(Value::Int(n)) => (*n as f32).to_bits() as i64,
            _ => 0,
        },
        ELEM_DOUBLE => match args.get(idx) {
            Some(Value::Double(f)) => f.to_bits() as i64,
            Some(Value::Long(n)) => (*n as f64).to_bits() as i64,
            Some(Value::Int(n)) => (*n as f64).to_bits() as i64,
            _ => 0,
        },
        _ => 0,
    }
}

pub(crate) fn array_lane_bits(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    index: usize,
    elem_type: u8,
) -> i64 {
    if index >= ctx.array_length(arr) {
        return 0;
    }
    match (elem_type, ctx.get_array_element(arr, index)) {
        (ELEM_BYTE, Value::Int(n)) => n as i8 as i64,
        (ELEM_SHORT, Value::Int(n)) => n as i16 as i64,
        (ELEM_INT, Value::Int(n)) => n as i64,
        (ELEM_LONG, Value::Long(n)) => n,
        (ELEM_LONG, Value::Int(n)) => n as i64,
        (ELEM_FLOAT, Value::Float(f)) => f.to_bits() as i64,
        (ELEM_FLOAT, Value::Int(n)) => n as u32 as i64,
        (ELEM_DOUBLE, Value::Double(f)) => f.to_bits() as i64,
        (ELEM_DOUBLE, Value::Long(n)) => n,
        (ELEM_DOUBLE, Value::Int(n)) => n as u64 as i64,
        (_, Value::Int(n)) => n as i64,
        (_, Value::Long(n)) => n,
        _ => 0,
    }
}

pub(crate) fn lane_bits_to_value(elem_type: u8, bits: i64) -> Value {
    match elem_type {
        ELEM_BYTE => Value::Int(bits as i8 as i32),
        ELEM_SHORT => Value::Int(bits as i16 as i32),
        ELEM_INT => Value::Int(bits as i32),
        ELEM_LONG => Value::Long(bits),
        ELEM_FLOAT => Value::Float(f32::from_bits(bits as u32)),
        ELEM_DOUBLE => Value::Double(f64::from_bits(bits as u64)),
        _ => Value::Int(bits as i32),
    }
}

pub(crate) fn wrap_integral_lane(elem_type: u8, value: i64) -> i64 {
    match elem_type {
        ELEM_BYTE => value as i8 as i64,
        ELEM_SHORT => value as i16 as i64,
        ELEM_INT => value as i32 as i64,
        ELEM_LONG => value,
        _ => value,
    }
}

fn integral_lane_value(elem_type: u8, bits: i64) -> i64 {
    match elem_type {
        ELEM_BYTE => bits as i8 as i64,
        ELEM_SHORT => bits as i16 as i64,
        ELEM_INT => bits as i32 as i64,
        ELEM_LONG => bits,
        _ => bits,
    }
}

fn safe_div_i64(lhs: i64, rhs: i64) -> i64 {
    if rhs == 0 {
        lhs
    } else if lhs == i64::MIN && rhs == -1 {
        i64::MIN
    } else {
        lhs / rhs
    }
}

fn safe_abs_i64(value: i64) -> i64 {
    if value == i64::MIN {
        i64::MIN
    } else {
        value.abs()
    }
}

fn integral_binary_lane(elem_type: u8, op_code: i32, lhs_bits: i64, rhs_bits: i64) -> i64 {
    let lhs = integral_lane_value(elem_type, lhs_bits);
    let rhs = integral_lane_value(elem_type, rhs_bits);
    let result = match op_code {
        OP_ADD => lhs.wrapping_add(rhs),
        OP_SUB => lhs.wrapping_sub(rhs),
        OP_MUL => lhs.wrapping_mul(rhs),
        OP_DIV => safe_div_i64(lhs, rhs),
        OP_AND => lhs & rhs,
        OP_OR => lhs | rhs,
        OP_XOR => lhs ^ rhs,
        OP_MIN => lhs.min(rhs),
        OP_MAX => lhs.max(rhs),
        _ => lhs,
    };
    wrap_integral_lane(elem_type, result)
}

fn integral_unary_lane(elem_type: u8, op_code: i32, bits: i64) -> i64 {
    let value = integral_lane_value(elem_type, bits);
    let result = match op_code {
        OP_NEG => value.wrapping_neg(),
        OP_ABS => safe_abs_i64(value),
        OP_NOT => !value,
        _ => value,
    };
    wrap_integral_lane(elem_type, result)
}

fn float_binary_lane(op_code: i32, lhs_bits: i64, rhs_bits: i64) -> i64 {
    let lhs = f32::from_bits(lhs_bits as u32);
    let rhs = f32::from_bits(rhs_bits as u32);
    let result = match op_code {
        OP_ADD => lhs + rhs,
        OP_SUB => lhs - rhs,
        OP_MUL => lhs * rhs,
        OP_DIV => lhs / rhs,
        OP_MIN => lhs.min(rhs),
        OP_MAX => lhs.max(rhs),
        _ => lhs,
    };
    result.to_bits() as i64
}

fn float_unary_lane(op_code: i32, bits: i64) -> i64 {
    let value = f32::from_bits(bits as u32);
    let result = match op_code {
        OP_NEG => -value,
        OP_ABS => value.abs(),
        OP_SQRT => value.sqrt(),
        _ => value,
    };
    result.to_bits() as i64
}

fn double_binary_lane(op_code: i32, lhs_bits: i64, rhs_bits: i64) -> i64 {
    let lhs = f64::from_bits(lhs_bits as u64);
    let rhs = f64::from_bits(rhs_bits as u64);
    let result = match op_code {
        OP_ADD => lhs + rhs,
        OP_SUB => lhs - rhs,
        OP_MUL => lhs * rhs,
        OP_DIV => lhs / rhs,
        OP_MIN => lhs.min(rhs),
        OP_MAX => lhs.max(rhs),
        _ => lhs,
    };
    result.to_bits() as i64
}

fn double_unary_lane(op_code: i32, bits: i64) -> i64 {
    let value = f64::from_bits(bits as u64);
    let result = match op_code {
        OP_NEG => -value,
        OP_ABS => value.abs(),
        OP_SQRT => value.sqrt(),
        _ => value,
    };
    result.to_bits() as i64
}

fn binary_lane(elem_type: u8, op_code: i32, lhs_bits: i64, rhs_bits: i64) -> i64 {
    match elem_type {
        ELEM_FLOAT => float_binary_lane(op_code, lhs_bits, rhs_bits),
        ELEM_DOUBLE => double_binary_lane(op_code, lhs_bits, rhs_bits),
        _ => integral_binary_lane(elem_type, op_code, lhs_bits, rhs_bits),
    }
}

fn unary_lane(elem_type: u8, op_code: i32, bits: i64) -> i64 {
    match elem_type {
        ELEM_FLOAT => float_unary_lane(op_code, bits),
        ELEM_DOUBLE => double_unary_lane(op_code, bits),
        _ => integral_unary_lane(elem_type, op_code, bits),
    }
}

fn vector_zero(ctx: &mut dyn NativeContext, args: &[Value], class_name: &str) -> MethodCallResult {
    let si = species_idx_from_arg(ctx, args);
    let lc = lane_count_from_species(si);
    let lanes = vec![0; normalized_lane_count(lc)];
    let obj = alloc_vector_lanes(ctx, class_name, si, lc, &lanes, 0);
    Ok(Some(Value::Object(Some(obj?))))
}

fn vector_broadcast(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
) -> MethodCallResult {
    let si = species_idx_from_arg(ctx, args);
    let lc = lane_count_from_species(si);
    let lane = lane_arg_bits(args, 1, elem_type);
    let lanes = vec![lane; normalized_lane_count(lc)];
    let obj = alloc_vector_lanes(ctx, class_name, si, lc, &lanes, 1);
    Ok(Some(Value::Object(Some(obj?))))
}

fn vector_from_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
) -> MethodCallResult {
    let si = species_idx_from_arg(ctx, args);
    let lc = lane_count_from_species(si);
    let offset = int_arg(args, 2).max(0) as usize;
    let src = match args.get(1) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };
    let lanes = (0..normalized_lane_count(lc))
        .map(|i| src.map_or(0, |arr| array_lane_bits(ctx, arr, offset + i, elem_type)))
        .collect::<Vec<_>>();
    let obj = alloc_vector_lanes(ctx, class_name, si, lc, &lanes, 1);
    Ok(Some(Value::Object(Some(obj?))))
}

fn vector_binary_op_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
    op_code: i32,
    other_index: usize,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, other_index)?;
    let (si, lc, lhs, c1) = read_vector_lanes(ctx, this);
    let (_, _, rhs, c2) = read_vector_lanes(ctx, other);
    let lane_count = normalized_lane_count(lc);
    let lanes = (0..lane_count)
        .map(|i| {
            binary_lane(
                elem_type,
                op_code,
                lhs.get(i).copied().unwrap_or(0),
                rhs.get(i).copied().unwrap_or(0),
            )
        })
        .collect::<Vec<_>>();
    let obj = alloc_vector_lanes(ctx, class_name, si, lc, &lanes, c1 + c2 + 1);
    Ok(Some(Value::Object(Some(obj?))))
}

fn vector_binary_op(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
    op_code: i32,
) -> MethodCallResult {
    vector_binary_op_at(ctx, args, class_name, elem_type, op_code, 1)
}

fn vector_unary_op(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
    op_code: i32,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (si, lc, src, c) = read_vector_lanes(ctx, this);
    let lanes = src
        .iter()
        .copied()
        .map(|lane| unary_lane(elem_type, op_code, lane))
        .collect::<Vec<_>>();
    let obj = alloc_vector_lanes(ctx, class_name, si, lc, &lanes, c + 1);
    Ok(Some(Value::Object(Some(obj?))))
}

fn vector_lanewise_unary(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
) -> MethodCallResult {
    vector_unary_op(ctx, args, class_name, elem_type, int_arg(args, 1))
}

fn vector_lanewise_binary(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
) -> MethodCallResult {
    vector_binary_op_at(ctx, args, class_name, elem_type, int_arg(args, 1), 2)
}

fn reduce_integral_lanes(elem_type: u8, op_code: i32, lanes: &[i64]) -> i64 {
    if lanes.is_empty() {
        return 0;
    }
    let mut iter = lanes
        .iter()
        .copied()
        .map(|lane| integral_lane_value(elem_type, lane));
    match op_code {
        RED_ADD => wrap_integral_lane(
            elem_type,
            iter.fold(0i64, |acc, lane| acc.wrapping_add(lane)),
        ),
        RED_MUL => wrap_integral_lane(
            elem_type,
            iter.fold(1i64, |acc, lane| acc.wrapping_mul(lane)),
        ),
        RED_MIN => wrap_integral_lane(elem_type, iter.min().unwrap_or(0)),
        RED_MAX => wrap_integral_lane(elem_type, iter.max().unwrap_or(0)),
        RED_AND => wrap_integral_lane(elem_type, iter.fold(-1i64, |acc, lane| acc & lane)),
        RED_OR => wrap_integral_lane(elem_type, iter.fold(0i64, |acc, lane| acc | lane)),
        RED_XOR => wrap_integral_lane(elem_type, iter.fold(0i64, |acc, lane| acc ^ lane)),
        _ => wrap_integral_lane(elem_type, lanes[0]),
    }
}

fn vector_reduce(ctx: &mut dyn NativeContext, args: &[Value], elem_type: u8) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let op = int_arg(args, 1);
    let (_, _, lanes, _) = read_vector_lanes(ctx, this);
    let value = match elem_type {
        ELEM_FLOAT => {
            let floats = lanes
                .iter()
                .copied()
                .map(|lane| f32::from_bits(lane as u32));
            let result = match op {
                RED_ADD => floats.fold(0.0f32, |acc, lane| acc + lane),
                RED_MUL => floats.fold(1.0f32, |acc, lane| acc * lane),
                RED_MIN => floats.reduce(|a, b| a.min(b)).unwrap_or(0.0),
                RED_MAX => floats.reduce(|a, b| a.max(b)).unwrap_or(0.0),
                _ => f32::from_bits(lanes.first().copied().unwrap_or(0) as u32),
            };
            Value::Float(result)
        }
        ELEM_DOUBLE => {
            let doubles = lanes
                .iter()
                .copied()
                .map(|lane| f64::from_bits(lane as u64));
            let result = match op {
                RED_ADD => doubles.fold(0.0f64, |acc, lane| acc + lane),
                RED_MUL => doubles.fold(1.0f64, |acc, lane| acc * lane),
                RED_MIN => doubles.reduce(|a, b| a.min(b)).unwrap_or(0.0),
                RED_MAX => doubles.reduce(|a, b| a.max(b)).unwrap_or(0.0),
                _ => f64::from_bits(lanes.first().copied().unwrap_or(0) as u64),
            };
            Value::Double(result)
        }
        ELEM_LONG => Value::Long(reduce_integral_lanes(elem_type, op, &lanes)),
        _ => Value::Int(reduce_integral_lanes(elem_type, op, &lanes) as i32),
    };
    Ok(Some(value))
}

fn vector_lane(ctx: &mut dyn NativeContext, args: &[Value], elem_type: u8) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = int_arg(args, 1).max(0) as usize;
    let (_, _, lanes, _) = read_vector_lanes(ctx, this);
    Ok(Some(lane_bits_to_value(
        elem_type,
        lanes.get(index).copied().unwrap_or(0),
    )))
}

fn vector_with_lane(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = int_arg(args, 1).max(0) as usize;
    let lane = lane_arg_bits(args, 2, elem_type);
    let (si, lc, mut lanes, c) = read_vector_lanes(ctx, this);
    if index < lanes.len() {
        lanes[index] = lane;
    }
    let obj = alloc_vector_lanes(ctx, class_name, si, lc, &lanes, c + 1);
    Ok(Some(Value::Object(Some(obj?))))
}

fn vector_to_array(ctx: &mut dyn NativeContext, args: &[Value], elem_type: u8) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (_, _, lanes, _) = read_vector_lanes(ctx, this);
    let arr_type = match elem_type {
        ELEM_BYTE => ArrayElementType::Byte,
        ELEM_SHORT => ArrayElementType::Short,
        ELEM_INT => ArrayElementType::Int,
        ELEM_LONG => ArrayElementType::Long,
        ELEM_FLOAT => ArrayElementType::Float,
        ELEM_DOUBLE => ArrayElementType::Double,
        _ => ArrayElementType::Int,
    };
    let arr = ctx.new_array(arr_type, lanes.len());
    for (i, lane) in lanes.iter().copied().enumerate() {
        ctx.set_array_element(arr, i, lane_bits_to_value(elem_type, lane));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn vector_into_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    elem_type: u8,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let dest = obj_arg(args, 1)?;
    let offset = int_arg(args, 2).max(0) as usize;
    let (_, _, lanes, _) = read_vector_lanes(ctx, this);
    let dest_len = ctx.array_length(dest);
    for (i, lane) in lanes.iter().copied().enumerate() {
        let dest_index = offset + i;
        if dest_index < dest_len {
            ctx.set_array_element(dest, dest_index, lane_bits_to_value(elem_type, lane));
        }
    }
    Ok(None)
}

fn compare_lane(elem_type: u8, cmp_op: i32, lhs_bits: i64, rhs_bits: i64) -> bool {
    match elem_type {
        ELEM_FLOAT => {
            let lhs = f32::from_bits(lhs_bits as u32);
            let rhs = f32::from_bits(rhs_bits as u32);
            match cmp_op {
                CMP_EQ => lhs == rhs,
                CMP_NE => lhs != rhs,
                CMP_LT => lhs < rhs,
                CMP_LE => lhs <= rhs,
                CMP_GT => lhs > rhs,
                CMP_GE => lhs >= rhs,
                _ => false,
            }
        }
        ELEM_DOUBLE => {
            let lhs = f64::from_bits(lhs_bits as u64);
            let rhs = f64::from_bits(rhs_bits as u64);
            match cmp_op {
                CMP_EQ => lhs == rhs,
                CMP_NE => lhs != rhs,
                CMP_LT => lhs < rhs,
                CMP_LE => lhs <= rhs,
                CMP_GT => lhs > rhs,
                CMP_GE => lhs >= rhs,
                _ => false,
            }
        }
        _ => {
            let lhs = integral_lane_value(elem_type, lhs_bits);
            let rhs = integral_lane_value(elem_type, rhs_bits);
            match cmp_op {
                CMP_EQ => lhs == rhs,
                CMP_NE => lhs != rhs,
                CMP_LT => lhs < rhs,
                CMP_LE => lhs <= rhs,
                CMP_GT => lhs > rhs,
                CMP_GE => lhs >= rhs,
                _ => false,
            }
        }
    }
}

fn vector_compare_result(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    elem_type: u8,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let cmp_op = int_arg(args, 1);
    let other = obj_arg(args, 2)?;
    let (_, lc, lhs, _) = read_vector_lanes(ctx, this);
    let (_, _, rhs, _) = read_vector_lanes(ctx, other);
    let mask_lanes = (0..normalized_lane_count(lc))
        .map(|i| {
            compare_lane(
                elem_type,
                cmp_op,
                lhs.get(i).copied().unwrap_or(0),
                rhs.get(i).copied().unwrap_or(0),
            )
        })
        .collect::<Vec<_>>();
    let mask = alloc_mask_from_lanes(ctx, lc, &mask_lanes);
    Ok(Some(Value::Object(Some(mask?))))
}

fn vector_blend(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let mask = obj_arg(args, 2)?;
    let (si, lc, lhs, c1) = read_vector_lanes(ctx, this);
    let (_, _, rhs, c2) = read_vector_lanes(ctx, other);
    let mask = mask_lane_values(ctx, mask, normalized_lane_count(lc));
    let lanes = (0..normalized_lane_count(lc))
        .map(|i| {
            if mask.get(i).copied().unwrap_or(false) {
                rhs.get(i).copied().unwrap_or(0)
            } else {
                lhs.get(i).copied().unwrap_or(0)
            }
        })
        .collect::<Vec<_>>();
    let obj = alloc_vector_lanes(ctx, class_name, si, lc, &lanes, c1 + c2 + 1);
    let _ = elem_type;
    Ok(Some(Value::Object(Some(obj?))))
}

fn shuffle_lane_source(
    ctx: &mut dyn NativeContext,
    shuffle: ObjectRef,
    index: usize,
    lane_count: usize,
) -> usize {
    let pattern = match ctx.get_field(shuffle, 1) {
        Value::Int(n) => n,
        _ => 0,
    };
    match pattern {
        1 => lane_count.saturating_sub(1).saturating_sub(index),
        2 => 0,
        _ => index,
    }
}

fn vector_rearrange(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let shuffle = obj_arg(args, 1)?;
    let (si, lc, src, c) = read_vector_lanes(ctx, this);
    let lane_count = normalized_lane_count(lc);
    let lanes = (0..lane_count)
        .map(|i| {
            src.get(shuffle_lane_source(ctx, shuffle, i, lane_count))
                .copied()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let obj = alloc_vector_lanes(ctx, class_name, si, lc, &lanes, c + 1);
    Ok(Some(Value::Object(Some(obj?))))
}

fn vector_fma(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    elem_type: u8,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let c_vec = obj_arg(args, 2)?;
    let (si, lc, a_lanes, c1) = read_vector_lanes(ctx, this);
    let (_, _, b_lanes, c2) = read_vector_lanes(ctx, b);
    let (_, _, c_lanes, c3) = read_vector_lanes(ctx, c_vec);
    let lane_count = normalized_lane_count(lc);
    let lanes = (0..lane_count)
        .map(|i| match elem_type {
            ELEM_DOUBLE => {
                let a = f64::from_bits(a_lanes.get(i).copied().unwrap_or(0) as u64);
                let b = f64::from_bits(b_lanes.get(i).copied().unwrap_or(0) as u64);
                let c = f64::from_bits(c_lanes.get(i).copied().unwrap_or(0) as u64);
                a.mul_add(b, c).to_bits() as i64
            }
            _ => {
                let a = f32::from_bits(a_lanes.get(i).copied().unwrap_or(0) as u32);
                let b = f32::from_bits(b_lanes.get(i).copied().unwrap_or(0) as u32);
                let c = f32::from_bits(c_lanes.get(i).copied().unwrap_or(0) as u32);
                a.mul_add(b, c).to_bits() as i64
            }
        })
        .collect::<Vec<_>>();
    let obj = alloc_vector_lanes(ctx, class_name, si, lc, &lanes, c1 + c2 + c3 + 1);
    Ok(Some(Value::Object(Some(obj?))))
}

// ---------------------------------------------------------------------------
// VectorSpecies natives
// ---------------------------------------------------------------------------
const VS: &str = "jdk/incubator/vector/VectorSpecies";

// VectorSpecies synthetic: [0]=species_idx (Int), [1]=element_type (Int), [2]=bit_size (Int), [3]=lane_count (Int)
fn alloc_species(ctx: &mut dyn NativeContext, species_idx: i32) -> Result<ObjectRef, MethodCallFailed> {
    let cfg = get_species_config(species_idx as usize);
    let obj = try_alloc_concurrent_synthetic(ctx, VS, 4)?;
    ctx.set_field(obj, 0, Value::Int(species_idx));
    ctx.set_field(obj, 1, Value::Int(cfg.element_type as i32));
    ctx.set_field(obj, 2, Value::Int(cfg.bit_size as i32));
    ctx.set_field(obj, 3, Value::Int(cfg.lane_count as i32));
    Ok(obj)
}

fn vs_of_int(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_species(ctx, 2)?)))) // 256-bit Int preferred
}

fn vs_of_long(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_species(ctx, 6)?)))) // 256-bit Long
}

fn vs_of_float(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_species(ctx, 10)?)))) // 256-bit Float
}

fn vs_of_double(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_species(ctx, 14)?)))) // 256-bit Double
}

fn vs_of_byte(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_species(ctx, 18)?)))) // 256-bit Byte
}

fn vs_of_short(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_species(ctx, 22)?)))) // 256-bit Short
}

fn vs_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let lc = match ctx.get_field(this, 3) {
        Value::Int(n) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(lc)))
}

fn vs_vector_bit_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bs = match ctx.get_field(this, 2) {
        Value::Int(n) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(bs)))
}

fn vs_element_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let et = match ctx.get_field(this, 1) {
        Value::Int(n) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(et)))
}

fn vs_element_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let et = match ctx.get_field(this, 1) {
        Value::Int(n) => n as u8,
        _ => ELEM_INT,
    };
    Ok(Some(Value::Int(element_size_bits(et) as i32)))
}

// ---------------------------------------------------------------------------
// IntVector natives
// ---------------------------------------------------------------------------
const IV: &str = "jdk/incubator/vector/IntVector";

fn iv_zero(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_zero(ctx, args, IV)
}

fn iv_broadcast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_broadcast(ctx, args, IV, ELEM_INT)
}

fn iv_from_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_from_array(ctx, args, IV, ELEM_INT)
}

fn iv_binary_op(ctx: &mut dyn NativeContext, args: &[Value], op_code: i32) -> MethodCallResult {
    vector_binary_op(ctx, args, IV, ELEM_INT, op_code)
}

fn iv_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    iv_binary_op(ctx, args, OP_ADD)
}

fn iv_sub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    iv_binary_op(ctx, args, OP_SUB)
}

fn iv_mul(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    iv_binary_op(ctx, args, OP_MUL)
}

fn iv_div(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    iv_binary_op(ctx, args, OP_DIV)
}

fn iv_and(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    iv_binary_op(ctx, args, OP_AND)
}

fn iv_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    iv_binary_op(ctx, args, OP_OR)
}

fn iv_xor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    iv_binary_op(ctx, args, OP_XOR)
}

fn iv_neg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, IV, ELEM_INT, OP_NEG)
}

fn iv_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, IV, ELEM_INT, OP_ABS)
}

fn iv_lanewise_unary(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_lanewise_unary(ctx, args, IV, ELEM_INT)
}

fn iv_lanewise_binary(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_lanewise_binary(ctx, args, IV, ELEM_INT)
}

fn iv_reduce_lanes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_reduce(ctx, args, ELEM_INT)
}

fn iv_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_lane(ctx, args, ELEM_INT)
}

fn iv_with_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_with_lane(ctx, args, IV, ELEM_INT)
}

fn iv_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_to_array(ctx, args, ELEM_INT)
}

fn iv_into_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_into_array(ctx, args, ELEM_INT)
}

fn iv_species(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (si, _, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Object(Some(alloc_species(ctx, si)?))))
}

fn iv_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (_, lc, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Int(lc)))
}

fn iv_reinterpret_as_longs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (si, lc, lanes, count) = read_vector_lanes(ctx, this);
    let long_si = match si {
        0 => 4,
        1 => 5,
        2 => 6,
        3 => 7,
        _ => 6,
    };
    let long_lc = (lc / 2).max(1);
    let long_lanes = lanes
        .chunks(2)
        .map(|chunk| {
            let lo = chunk.get(0).copied().unwrap_or(0) as u32 as u64;
            let hi = chunk.get(1).copied().unwrap_or(0) as u32 as u64;
            ((hi << 32) | lo) as i64
        })
        .collect::<Vec<_>>();
    let obj = alloc_vector_lanes(ctx, LV, long_si, long_lc, &long_lanes, count);
    Ok(Some(Value::Object(Some(obj?))))
}

fn iv_blend(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_blend(ctx, args, IV, ELEM_INT)
}

fn iv_rearrange(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_rearrange(ctx, args, IV)
}

fn iv_compare(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_compare_result(ctx, args, ELEM_INT)
}

// ---------------------------------------------------------------------------
// LongVector natives
// ---------------------------------------------------------------------------
const LV: &str = "jdk/incubator/vector/LongVector";

fn lv_zero(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_zero(ctx, args, LV)
}

fn lv_broadcast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_broadcast(ctx, args, LV, ELEM_LONG)
}

fn lv_from_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_from_array(ctx, args, LV, ELEM_LONG)
}

fn lv_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lv_binary_op(ctx, args, OP_ADD)
}

fn lv_sub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lv_binary_op(ctx, args, OP_SUB)
}

fn lv_mul(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lv_binary_op(ctx, args, OP_MUL)
}

fn lv_and(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lv_binary_op(ctx, args, OP_AND)
}

fn lv_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lv_binary_op(ctx, args, OP_OR)
}

fn lv_xor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lv_binary_op(ctx, args, OP_XOR)
}

fn lv_binary_op(ctx: &mut dyn NativeContext, args: &[Value], op_code: i32) -> MethodCallResult {
    vector_binary_op(ctx, args, LV, ELEM_LONG, op_code)
}

fn lv_neg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, LV, ELEM_LONG, OP_NEG)
}

fn lv_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, LV, ELEM_LONG, OP_ABS)
}

fn lv_reduce_lanes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_reduce(ctx, args, ELEM_LONG)
}

fn lv_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_lane(ctx, args, ELEM_LONG)
}

fn lv_with_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_with_lane(ctx, args, LV, ELEM_LONG)
}

fn lv_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (_, lc, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Int(lc)))
}

fn lv_species(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (si, _, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Object(Some(alloc_species(ctx, si)?))))
}

// ---------------------------------------------------------------------------
// FloatVector natives
// ---------------------------------------------------------------------------
const FV: &str = "jdk/incubator/vector/FloatVector";

fn fv_zero(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_zero(ctx, args, FV)
}

fn fv_broadcast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_broadcast(ctx, args, FV, ELEM_FLOAT)
}

fn fv_from_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_from_array(ctx, args, FV, ELEM_FLOAT)
}

fn fv_binary_op(ctx: &mut dyn NativeContext, args: &[Value], op_code: i32) -> MethodCallResult {
    vector_binary_op(ctx, args, FV, ELEM_FLOAT, op_code)
}

fn fv_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    fv_binary_op(ctx, args, OP_ADD)
}

fn fv_sub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    fv_binary_op(ctx, args, OP_SUB)
}

fn fv_mul(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    fv_binary_op(ctx, args, OP_MUL)
}

fn fv_div(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    fv_binary_op(ctx, args, OP_DIV)
}

fn fv_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    fv_binary_op(ctx, args, OP_MIN)
}

fn fv_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    fv_binary_op(ctx, args, OP_MAX)
}

fn fv_neg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, FV, ELEM_FLOAT, OP_NEG)
}

fn fv_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, FV, ELEM_FLOAT, OP_ABS)
}

fn fv_sqrt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, FV, ELEM_FLOAT, OP_SQRT)
}

fn fv_fma(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_fma(ctx, args, FV, ELEM_FLOAT)
}

fn fv_reduce_lanes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_reduce(ctx, args, ELEM_FLOAT)
}

fn fv_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_lane(ctx, args, ELEM_FLOAT)
}

fn fv_with_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_with_lane(ctx, args, FV, ELEM_FLOAT)
}

fn fv_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (_, lc, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Int(lc)))
}

fn fv_species(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (si, _, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Object(Some(alloc_species(ctx, si)?))))
}

// ---------------------------------------------------------------------------
// DoubleVector natives
// ---------------------------------------------------------------------------
const DV: &str = "jdk/incubator/vector/DoubleVector";

fn dv_zero(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_zero(ctx, args, DV)
}

fn dv_broadcast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_broadcast(ctx, args, DV, ELEM_DOUBLE)
}

fn dv_from_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_from_array(ctx, args, DV, ELEM_DOUBLE)
}

fn dv_binary_op(ctx: &mut dyn NativeContext, args: &[Value], op_code: i32) -> MethodCallResult {
    vector_binary_op(ctx, args, DV, ELEM_DOUBLE, op_code)
}

fn dv_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dv_binary_op(ctx, args, OP_ADD)
}

fn dv_sub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dv_binary_op(ctx, args, OP_SUB)
}

fn dv_mul(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dv_binary_op(ctx, args, OP_MUL)
}

fn dv_div(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dv_binary_op(ctx, args, OP_DIV)
}

fn dv_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dv_binary_op(ctx, args, OP_MIN)
}

fn dv_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dv_binary_op(ctx, args, OP_MAX)
}

fn dv_neg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, DV, ELEM_DOUBLE, OP_NEG)
}

fn dv_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, DV, ELEM_DOUBLE, OP_ABS)
}

fn dv_sqrt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, DV, ELEM_DOUBLE, OP_SQRT)
}

fn dv_fma(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_fma(ctx, args, DV, ELEM_DOUBLE)
}

fn dv_reduce_lanes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_reduce(ctx, args, ELEM_DOUBLE)
}

fn dv_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_lane(ctx, args, ELEM_DOUBLE)
}

fn dv_with_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_with_lane(ctx, args, DV, ELEM_DOUBLE)
}

fn dv_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (_, lc, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Int(lc)))
}

fn dv_species(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (si, _, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Object(Some(alloc_species(ctx, si)?))))
}

// ---------------------------------------------------------------------------
// ByteVector natives
// ---------------------------------------------------------------------------
const BV: &str = "jdk/incubator/vector/ByteVector";

fn bv_zero(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_zero(ctx, args, BV)
}

fn bv_broadcast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_broadcast(ctx, args, BV, ELEM_BYTE)
}

fn bv_from_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_from_array(ctx, args, BV, ELEM_BYTE)
}

fn bv_binary_op(ctx: &mut dyn NativeContext, args: &[Value], op_code: i32) -> MethodCallResult {
    vector_binary_op(ctx, args, BV, ELEM_BYTE, op_code)
}

fn bv_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    bv_binary_op(ctx, args, OP_ADD)
}

fn bv_sub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    bv_binary_op(ctx, args, OP_SUB)
}

fn bv_mul(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    bv_binary_op(ctx, args, OP_MUL)
}

fn bv_and(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    bv_binary_op(ctx, args, OP_AND)
}

fn bv_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    bv_binary_op(ctx, args, OP_OR)
}

fn bv_xor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    bv_binary_op(ctx, args, OP_XOR)
}

fn bv_neg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, BV, ELEM_BYTE, OP_NEG)
}

fn bv_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, BV, ELEM_BYTE, OP_ABS)
}

fn bv_reduce_lanes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_reduce(ctx, args, ELEM_BYTE)
}

fn bv_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_lane(ctx, args, ELEM_BYTE)
}

fn bv_with_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_with_lane(ctx, args, BV, ELEM_BYTE)
}

fn bv_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (_, lc, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Int(lc)))
}

fn bv_species(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (si, _, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Object(Some(alloc_species(ctx, si)?))))
}

// ---------------------------------------------------------------------------
// ShortVector natives
// ---------------------------------------------------------------------------
const SV_VEC: &str = "jdk/incubator/vector/ShortVector";
const VECTOR_SUPPORT: &str = "jdk/internal/vm/vector/VectorSupport";

fn sv_vec_zero(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_zero(ctx, args, SV_VEC)
}

fn sv_vec_broadcast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_broadcast(ctx, args, SV_VEC, ELEM_SHORT)
}

fn sv_vec_from_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_from_array(ctx, args, SV_VEC, ELEM_SHORT)
}

fn sv_vec_binary_op(ctx: &mut dyn NativeContext, args: &[Value], op_code: i32) -> MethodCallResult {
    vector_binary_op(ctx, args, SV_VEC, ELEM_SHORT, op_code)
}

fn sv_vec_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sv_vec_binary_op(ctx, args, OP_ADD)
}

fn sv_vec_sub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sv_vec_binary_op(ctx, args, OP_SUB)
}

fn sv_vec_mul(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sv_vec_binary_op(ctx, args, OP_MUL)
}

fn sv_vec_and(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sv_vec_binary_op(ctx, args, OP_AND)
}

fn sv_vec_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sv_vec_binary_op(ctx, args, OP_OR)
}

fn sv_vec_xor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sv_vec_binary_op(ctx, args, OP_XOR)
}

fn sv_vec_neg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, SV_VEC, ELEM_SHORT, OP_NEG)
}

fn sv_vec_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_unary_op(ctx, args, SV_VEC, ELEM_SHORT, OP_ABS)
}

fn sv_vec_reduce_lanes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_reduce(ctx, args, ELEM_SHORT)
}

fn sv_vec_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_lane(ctx, args, ELEM_SHORT)
}

fn sv_vec_with_lane(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    vector_with_lane(ctx, args, SV_VEC, ELEM_SHORT)
}

fn sv_vec_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (_, lc, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Int(lc)))
}

fn sv_vec_species(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (si, _, _, _) = read_vector_fields(ctx, this);
    Ok(Some(Value::Object(Some(alloc_species(ctx, si)?))))
}

// ---------------------------------------------------------------------------
// Cross-type conversion functions (IntVector)
// ---------------------------------------------------------------------------

/// `IntVector.convertShape(ILjdk/incubator/vector/VectorSpecies;I)Ljdk/incubator/vector/Vector;`
fn iv_convert_shape(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let conv_op = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let (si, lc, lanes, c) = read_vector_lanes(ctx, this);
    // Conversion ops: 0=ZERO_EXTEND, 1=SIGN_EXTEND, 2=NARROW, 3=FLOAT_TO_INT, 4=INT_TO_FLOAT
    let target_class = match conv_op {
        0 | 1 => LV, // widen int -> long
        3 => IV,     // float -> int stays int
        4 => FV,     // int -> float
        _ => IV,
    };
    let converted = match conv_op {
        0 => lanes
            .iter()
            .copied()
            .map(|lane| lane as u32 as i64)
            .collect::<Vec<_>>(),
        4 => lanes
            .iter()
            .copied()
            .map(|lane| (lane as i32 as f32).to_bits() as i64)
            .collect::<Vec<_>>(),
        _ => lanes,
    };
    let obj = alloc_vector_lanes(ctx, target_class, si, lc, &converted, c + 1);
    Ok(Some(Value::Object(Some(obj?))))
}

/// `IntVector.castShape(Ljdk/incubator/vector/VectorSpecies;I)Ljdk/incubator/vector/Vector;`
fn iv_cast_shape(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (_, _, lanes, c) = read_vector_lanes(ctx, this);
    // Cast to target species element type
    let target_si = species_idx_from_arg(ctx, &args[1..]);
    let target_lc = lane_count_from_species(target_si);
    let target_elem = get_species_config(target_si as usize).element_type;
    let target_class = match target_elem {
        ELEM_LONG => LV,
        ELEM_FLOAT => FV,
        ELEM_DOUBLE => DV,
        ELEM_BYTE => BV,
        ELEM_SHORT => SV_VEC,
        _ => IV,
    };
    let obj = alloc_vector_lanes(ctx, target_class, target_si, target_lc, &lanes, c + 1);
    Ok(Some(Value::Object(Some(obj?))))
}

// ---------------------------------------------------------------------------
// VectorMask natives
// ---------------------------------------------------------------------------
const VM: &str = "jdk/incubator/vector/VectorMask";

// VectorMask synthetic: [0]=lane_count (Int), [1]=true_count (Int), [2]=lane payload (long[] 0/1)
fn alloc_mask(ctx: &mut dyn NativeContext, lane_count: i32, true_count: i32) -> Result<ObjectRef, MethodCallFailed> {
    let lanes = (0..normalized_lane_count(lane_count))
        .map(|i| (i as i32) < true_count)
        .collect::<Vec<_>>();
    Ok(alloc_mask_from_lanes(ctx, lane_count, &lanes)?)
}

fn alloc_mask_from_lanes(
    ctx: &mut dyn NativeContext,
    lane_count: i32,
    lanes: &[bool],
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, VM, 3)?;
    let payload_len = normalized_lane_count(lane_count);
    let payload = ctx.new_array(ArrayElementType::Long, payload_len);
    let mut true_count = 0i32;
    for i in 0..payload_len {
        let set = lanes.get(i).copied().unwrap_or(false);
        if set {
            true_count += 1;
        }
        ctx.set_array_element(payload, i, Value::Long(if set { 1 } else { 0 }));
    }
    ctx.set_field(obj, 0, Value::Int(lane_count));
    ctx.set_field(obj, 1, Value::Int(true_count));
    ctx.set_field(obj, 2, Value::Object(Some(payload)));
    Ok(obj)
}

fn mask_lane_values(ctx: &mut dyn NativeContext, mask: ObjectRef, lane_count: usize) -> Vec<bool> {
    match ctx.get_field(mask, 2) {
        Value::Object(Some(payload)) => {
            let payload_len = ctx.array_length(payload);
            (0..lane_count)
                .map(|i| {
                    if i < payload_len {
                        match ctx.get_array_element(payload, i) {
                            Value::Long(n) => n != 0,
                            Value::Int(n) => n != 0,
                            _ => false,
                        }
                    } else {
                        false
                    }
                })
                .collect()
        }
        _ => {
            let true_count = match ctx.get_field(mask, 1) {
                Value::Int(n) => n.max(0) as usize,
                _ => 0,
            };
            (0..lane_count).map(|i| i < true_count).collect()
        }
    }
}

fn vm_from_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lane_count = match args.get(0) {
        Some(Value::Int(n)) => *n,
        _ => 8,
    };
    let true_count = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let mask = alloc_mask(ctx, lane_count, true_count);
    Ok(Some(Value::Object(Some(mask?))))
}

fn vm_all_true(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lane_count = match args.get(0) {
        Some(Value::Int(n)) => *n,
        _ => 8,
    };
    let mask = alloc_mask(ctx, lane_count, lane_count);
    Ok(Some(Value::Object(Some(mask?))))
}

fn vm_all_false(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lane_count = match args.get(0) {
        Some(Value::Int(n)) => *n,
        _ => 8,
    };
    let mask = alloc_mask(ctx, lane_count, 0);
    Ok(Some(Value::Object(Some(mask?))))
}

fn vm_true_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let tc = match ctx.get_field(this, 1) {
        Value::Int(n) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(tc)))
}

fn vm_lane_is_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let idx = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let lc = match ctx.get_field(this, 0) {
        Value::Int(n) => normalized_lane_count(n),
        _ => 0,
    };
    let lanes = mask_lane_values(ctx, this, lc);
    let is_set = usize::try_from(idx)
        .ok()
        .and_then(|i| lanes.get(i).copied())
        .unwrap_or(false);
    Ok(Some(Value::Int(if is_set { 1 } else { 0 })))
}

fn vm_and(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let lc = match ctx.get_field(this, 0) {
        Value::Int(n) => n,
        _ => 0,
    };
    let lane_count = normalized_lane_count(lc);
    let lhs = mask_lane_values(ctx, this, lane_count);
    let rhs = mask_lane_values(ctx, other, lane_count);
    let lanes = (0..lane_count)
        .map(|i| lhs.get(i).copied().unwrap_or(false) && rhs.get(i).copied().unwrap_or(false))
        .collect::<Vec<_>>();
    let mask = alloc_mask_from_lanes(ctx, lc, &lanes);
    Ok(Some(Value::Object(Some(mask?))))
}

fn vm_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let lc = match ctx.get_field(this, 0) {
        Value::Int(n) => n,
        _ => 0,
    };
    let lane_count = normalized_lane_count(lc);
    let lhs = mask_lane_values(ctx, this, lane_count);
    let rhs = mask_lane_values(ctx, other, lane_count);
    let lanes = (0..lane_count)
        .map(|i| lhs.get(i).copied().unwrap_or(false) || rhs.get(i).copied().unwrap_or(false))
        .collect::<Vec<_>>();
    let mask = alloc_mask_from_lanes(ctx, lc, &lanes);
    Ok(Some(Value::Object(Some(mask?))))
}

fn vm_not(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let lc = match ctx.get_field(this, 0) {
        Value::Int(n) => n,
        _ => 0,
    };
    let lane_count = normalized_lane_count(lc);
    let lanes = mask_lane_values(ctx, this, lane_count)
        .into_iter()
        .map(|set| !set)
        .collect::<Vec<_>>();
    let mask = alloc_mask_from_lanes(ctx, lc, &lanes);
    Ok(Some(Value::Object(Some(mask?))))
}

fn vm_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let lc = match ctx.get_field(this, 0) {
        Value::Int(n) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(lc)))
}

// ---------------------------------------------------------------------------
// VectorShuffle natives
// ---------------------------------------------------------------------------
const VSH: &str = "jdk/incubator/vector/VectorShuffle";

// VectorShuffle synthetic: [0]=lane_count (Int), [1]=pattern (Int: 0=identity, 1=reverse, 2=broadcast0)
fn alloc_shuffle(ctx: &mut dyn NativeContext, lane_count: i32, pattern: i32) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, VSH, 2)?;
    ctx.set_field(obj, 0, Value::Int(lane_count));
    ctx.set_field(obj, 1, Value::Int(pattern));
    Ok(obj)
}

fn vsh_from_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lane_count = match args.get(0) {
        Some(Value::Int(n)) => *n,
        _ => 8,
    };
    let pattern = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let shuf = alloc_shuffle(ctx, lane_count, pattern);
    Ok(Some(Value::Object(Some(shuf?))))
}

fn vsh_iota(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lane_count = match args.get(0) {
        Some(Value::Int(n)) => *n,
        _ => 8,
    };
    let shuf = alloc_shuffle(ctx, lane_count, 0); // identity = iota
    Ok(Some(Value::Object(Some(shuf?))))
}

fn vsh_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let lc = match ctx.get_field(this, 0) {
        Value::Int(n) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(lc)))
}

fn vsh_lane_source(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let idx = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let lc = match ctx.get_field(this, 0) {
        Value::Int(n) => n,
        _ => 8,
    };
    let pattern = match ctx.get_field(this, 1) {
        Value::Int(n) => n,
        _ => 0,
    };
    let source = match pattern {
        0 => idx,            // identity
        1 => (lc - 1) - idx, // reverse
        2 => 0,              // broadcast lane 0
        _ => idx,
    };
    Ok(Some(Value::Int(source)))
}

// ---------------------------------------------------------------------------
// VectorOperators natives
// ---------------------------------------------------------------------------
const VO: &str = "jdk/incubator/vector/VectorOperators";

fn vo_op_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = match args.get(0) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    };
    let name = op_name(code);
    let s = ctx.create_string(name);
    Ok(Some(Value::Object(Some(s))))
}

fn vo_is_associative(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = match args.get(0) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    };
    Ok(Some(Value::Int(if is_associative(code) { 1 } else { 0 })))
}

fn vector_support_register_natives(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn vector_support_get_cpu_features(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let features = ctx.create_string("");
    Ok(Some(Value::Object(Some(features))))
}

fn vector_support_get_max_lane_count(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // JDK 25's VectorShape.S_Max_BIT derives its bit size from
    // getMaxLaneCount(byte.class) * Byte.SIZE. CratonVM's vector bridge is a
    // scalar-correct implementation, so expose the minimum 128-bit byte-lane
    // count instead of disabling Lucene's vector path with a spurious 64-bit
    // preferred shape.
    Ok(Some(Value::Int(16)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub(crate) fn register_vector_api_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_vector_support_natives(r);
    register_vector_species(r);
    register_int_vector(r);
    register_long_vector(r);
    register_float_vector(r);
    register_double_vector(r);
    register_byte_vector(r);
    register_short_vector(r);
    register_vector_mask(r);
    register_vector_shuffle(r);
    register_vector_operators(r);
    r.set_category(__prev_cat);
}

pub(crate) fn register_vector_support_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register_with_kind(
        VECTOR_SUPPORT,
        "registerNatives",
        "()I",
        vector_support_register_natives,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "getCPUFeatures",
        "()Ljava/lang/String;",
        vector_support_get_cpu_features,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        VECTOR_SUPPORT,
        "getMaxLaneCount",
        "(Ljava/lang/Class;)I",
        vector_support_get_max_lane_count,
        NativeKind::Bridge,
    );
    r.set_category(__prev_cat);
}

fn register_vector_species(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        VS,
        "ofInt",
        "()Ljdk/incubator/vector/VectorSpecies;",
        vs_of_int,
    );
    r.register(
        VS,
        "ofLong",
        "()Ljdk/incubator/vector/VectorSpecies;",
        vs_of_long,
    );
    r.register(
        VS,
        "ofFloat",
        "()Ljdk/incubator/vector/VectorSpecies;",
        vs_of_float,
    );
    r.register(
        VS,
        "ofDouble",
        "()Ljdk/incubator/vector/VectorSpecies;",
        vs_of_double,
    );
    r.register(
        VS,
        "ofByte",
        "()Ljdk/incubator/vector/VectorSpecies;",
        vs_of_byte,
    );
    r.register(
        VS,
        "ofShort",
        "()Ljdk/incubator/vector/VectorSpecies;",
        vs_of_short,
    );
    r.register(VS, "length", "()I", vs_length);
    r.register(VS, "vectorBitSize", "()I", vs_vector_bit_size);
    r.register(VS, "elementType", "()I", vs_element_type);
    r.register(VS, "elementSize", "()I", vs_element_size);
    r.set_category(__prev_cat);
}

fn register_int_vector(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        IV,
        "zero",
        "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/IntVector;",
        iv_zero,
    );
    r.register(
        IV,
        "broadcast",
        "(Ljdk/incubator/vector/VectorSpecies;I)Ljdk/incubator/vector/IntVector;",
        iv_broadcast,
    );
    r.register(
        IV,
        "fromArray",
        "(Ljdk/incubator/vector/VectorSpecies;[II)Ljdk/incubator/vector/IntVector;",
        iv_from_array,
    );
    r.register(
        IV,
        "add",
        "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;",
        iv_add,
    );
    r.register(
        IV,
        "sub",
        "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;",
        iv_sub,
    );
    r.register(
        IV,
        "mul",
        "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;",
        iv_mul,
    );
    r.register(
        IV,
        "div",
        "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;",
        iv_div,
    );
    r.register(
        IV,
        "and",
        "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;",
        iv_and,
    );
    r.register(
        IV,
        "or",
        "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;",
        iv_or,
    );
    r.register(
        IV,
        "xor",
        "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;",
        iv_xor,
    );
    r.register(IV, "neg", "()Ljdk/incubator/vector/IntVector;", iv_neg);
    r.register(IV, "abs", "()Ljdk/incubator/vector/IntVector;", iv_abs);
    r.register(
        IV,
        "lanewise",
        "(I)Ljdk/incubator/vector/IntVector;",
        iv_lanewise_unary,
    );
    r.register(
        IV,
        "lanewise",
        "(ILjdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;",
        iv_lanewise_binary,
    );
    r.register(IV, "reduceLanes", "(I)I", iv_reduce_lanes);
    r.register(IV, "lane", "(I)I", iv_lane);
    r.register(
        IV,
        "withLane",
        "(II)Ljdk/incubator/vector/IntVector;",
        iv_with_lane,
    );
    r.register(IV, "toArray", "()[I", iv_to_array);
    r.register(IV, "intoArray", "([II)V", iv_into_array);
    r.register(
        IV,
        "species",
        "()Ljdk/incubator/vector/VectorSpecies;",
        iv_species,
    );
    r.register(IV, "length", "()I", iv_length);
    r.register(
        IV,
        "reinterpretAsLongs",
        "()Ljdk/incubator/vector/LongVector;",
        iv_reinterpret_as_longs,
    );
    r.register(IV, "blend", "(Ljdk/incubator/vector/IntVector;Ljdk/incubator/vector/VectorMask;)Ljdk/incubator/vector/IntVector;", iv_blend);
    r.register(
        IV,
        "rearrange",
        "(Ljdk/incubator/vector/VectorShuffle;)Ljdk/incubator/vector/IntVector;",
        iv_rearrange,
    );
    r.register(
        IV,
        "compare",
        "(ILjdk/incubator/vector/IntVector;)Ljdk/incubator/vector/VectorMask;",
        iv_compare,
    );
    r.register(
        IV,
        "convertShape",
        "(ILjdk/incubator/vector/VectorSpecies;I)Ljdk/incubator/vector/Vector;",
        iv_convert_shape,
    );
    r.register(
        IV,
        "castShape",
        "(Ljdk/incubator/vector/VectorSpecies;I)Ljdk/incubator/vector/Vector;",
        iv_cast_shape,
    );
    r.set_category(__prev_cat);
}

fn register_long_vector(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        LV,
        "zero",
        "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/LongVector;",
        lv_zero,
    );
    r.register(
        LV,
        "broadcast",
        "(Ljdk/incubator/vector/VectorSpecies;J)Ljdk/incubator/vector/LongVector;",
        lv_broadcast,
    );
    r.register(
        LV,
        "fromArray",
        "(Ljdk/incubator/vector/VectorSpecies;[JI)Ljdk/incubator/vector/LongVector;",
        lv_from_array,
    );
    r.register(
        LV,
        "add",
        "(Ljdk/incubator/vector/LongVector;)Ljdk/incubator/vector/LongVector;",
        lv_add,
    );
    r.register(
        LV,
        "sub",
        "(Ljdk/incubator/vector/LongVector;)Ljdk/incubator/vector/LongVector;",
        lv_sub,
    );
    r.register(
        LV,
        "mul",
        "(Ljdk/incubator/vector/LongVector;)Ljdk/incubator/vector/LongVector;",
        lv_mul,
    );
    r.register(
        LV,
        "and",
        "(Ljdk/incubator/vector/LongVector;)Ljdk/incubator/vector/LongVector;",
        lv_and,
    );
    r.register(
        LV,
        "or",
        "(Ljdk/incubator/vector/LongVector;)Ljdk/incubator/vector/LongVector;",
        lv_or,
    );
    r.register(
        LV,
        "xor",
        "(Ljdk/incubator/vector/LongVector;)Ljdk/incubator/vector/LongVector;",
        lv_xor,
    );
    r.register(LV, "neg", "()Ljdk/incubator/vector/LongVector;", lv_neg);
    r.register(LV, "abs", "()Ljdk/incubator/vector/LongVector;", lv_abs);
    r.register(LV, "reduceLanes", "(I)J", lv_reduce_lanes);
    r.register(LV, "lane", "(I)J", lv_lane);
    r.register(
        LV,
        "withLane",
        "(IJ)Ljdk/incubator/vector/LongVector;",
        lv_with_lane,
    );
    r.register(LV, "length", "()I", lv_length);
    r.register(
        LV,
        "species",
        "()Ljdk/incubator/vector/VectorSpecies;",
        lv_species,
    );
    r.set_category(__prev_cat);
}

fn register_float_vector(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        FV,
        "zero",
        "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/FloatVector;",
        fv_zero,
    );
    r.register(
        FV,
        "broadcast",
        "(Ljdk/incubator/vector/VectorSpecies;F)Ljdk/incubator/vector/FloatVector;",
        fv_broadcast,
    );
    r.register(
        FV,
        "fromArray",
        "(Ljdk/incubator/vector/VectorSpecies;[FI)Ljdk/incubator/vector/FloatVector;",
        fv_from_array,
    );
    r.register(
        FV,
        "add",
        "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;",
        fv_add,
    );
    r.register(
        FV,
        "sub",
        "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;",
        fv_sub,
    );
    r.register(
        FV,
        "mul",
        "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;",
        fv_mul,
    );
    r.register(
        FV,
        "div",
        "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;",
        fv_div,
    );
    r.register(FV, "neg", "()Ljdk/incubator/vector/FloatVector;", fv_neg);
    r.register(FV, "abs", "()Ljdk/incubator/vector/FloatVector;", fv_abs);
    r.register(FV, "sqrt", "()Ljdk/incubator/vector/FloatVector;", fv_sqrt);
    r.register(FV, "fma", "(Ljdk/incubator/vector/FloatVector;Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;", fv_fma);
    r.register(FV, "reduceLanes", "(I)F", fv_reduce_lanes);
    r.register(FV, "lane", "(I)F", fv_lane);
    r.register(
        FV,
        "withLane",
        "(IF)Ljdk/incubator/vector/FloatVector;",
        fv_with_lane,
    );
    r.register(FV, "length", "()I", fv_length);
    r.register(
        FV,
        "species",
        "()Ljdk/incubator/vector/VectorSpecies;",
        fv_species,
    );
    r.register(
        FV,
        "min",
        "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;",
        fv_min,
    );
    r.register(
        FV,
        "max",
        "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;",
        fv_max,
    );
    r.set_category(__prev_cat);
}

fn register_double_vector(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        DV,
        "zero",
        "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/DoubleVector;",
        dv_zero,
    );
    r.register(
        DV,
        "broadcast",
        "(Ljdk/incubator/vector/VectorSpecies;D)Ljdk/incubator/vector/DoubleVector;",
        dv_broadcast,
    );
    r.register(
        DV,
        "fromArray",
        "(Ljdk/incubator/vector/VectorSpecies;[DI)Ljdk/incubator/vector/DoubleVector;",
        dv_from_array,
    );
    r.register(
        DV,
        "add",
        "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;",
        dv_add,
    );
    r.register(
        DV,
        "sub",
        "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;",
        dv_sub,
    );
    r.register(
        DV,
        "mul",
        "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;",
        dv_mul,
    );
    r.register(
        DV,
        "div",
        "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;",
        dv_div,
    );
    r.register(DV, "neg", "()Ljdk/incubator/vector/DoubleVector;", dv_neg);
    r.register(DV, "abs", "()Ljdk/incubator/vector/DoubleVector;", dv_abs);
    r.register(DV, "sqrt", "()Ljdk/incubator/vector/DoubleVector;", dv_sqrt);
    r.register(DV, "fma", "(Ljdk/incubator/vector/DoubleVector;Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;", dv_fma);
    r.register(DV, "reduceLanes", "(I)D", dv_reduce_lanes);
    r.register(DV, "lane", "(I)D", dv_lane);
    r.register(
        DV,
        "withLane",
        "(ID)Ljdk/incubator/vector/DoubleVector;",
        dv_with_lane,
    );
    r.register(DV, "length", "()I", dv_length);
    r.register(
        DV,
        "species",
        "()Ljdk/incubator/vector/VectorSpecies;",
        dv_species,
    );
    r.register(
        DV,
        "min",
        "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;",
        dv_min,
    );
    r.register(
        DV,
        "max",
        "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;",
        dv_max,
    );
    r.set_category(__prev_cat);
}

fn register_byte_vector(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        BV,
        "zero",
        "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/ByteVector;",
        bv_zero,
    );
    r.register(
        BV,
        "broadcast",
        "(Ljdk/incubator/vector/VectorSpecies;B)Ljdk/incubator/vector/ByteVector;",
        bv_broadcast,
    );
    r.register(
        BV,
        "fromArray",
        "(Ljdk/incubator/vector/VectorSpecies;[BI)Ljdk/incubator/vector/ByteVector;",
        bv_from_array,
    );
    r.register(
        BV,
        "add",
        "(Ljdk/incubator/vector/ByteVector;)Ljdk/incubator/vector/ByteVector;",
        bv_add,
    );
    r.register(
        BV,
        "sub",
        "(Ljdk/incubator/vector/ByteVector;)Ljdk/incubator/vector/ByteVector;",
        bv_sub,
    );
    r.register(
        BV,
        "mul",
        "(Ljdk/incubator/vector/ByteVector;)Ljdk/incubator/vector/ByteVector;",
        bv_mul,
    );
    r.register(
        BV,
        "and",
        "(Ljdk/incubator/vector/ByteVector;)Ljdk/incubator/vector/ByteVector;",
        bv_and,
    );
    r.register(
        BV,
        "or",
        "(Ljdk/incubator/vector/ByteVector;)Ljdk/incubator/vector/ByteVector;",
        bv_or,
    );
    r.register(
        BV,
        "xor",
        "(Ljdk/incubator/vector/ByteVector;)Ljdk/incubator/vector/ByteVector;",
        bv_xor,
    );
    r.register(BV, "neg", "()Ljdk/incubator/vector/ByteVector;", bv_neg);
    r.register(BV, "abs", "()Ljdk/incubator/vector/ByteVector;", bv_abs);
    r.register(BV, "reduceLanes", "(I)B", bv_reduce_lanes);
    r.register(BV, "lane", "(I)B", bv_lane);
    r.register(
        BV,
        "withLane",
        "(IB)Ljdk/incubator/vector/ByteVector;",
        bv_with_lane,
    );
    r.register(BV, "length", "()I", bv_length);
    r.register(
        BV,
        "species",
        "()Ljdk/incubator/vector/VectorSpecies;",
        bv_species,
    );
    r.set_category(__prev_cat);
}

fn register_short_vector(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        SV_VEC,
        "zero",
        "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/ShortVector;",
        sv_vec_zero,
    );
    r.register(
        SV_VEC,
        "broadcast",
        "(Ljdk/incubator/vector/VectorSpecies;S)Ljdk/incubator/vector/ShortVector;",
        sv_vec_broadcast,
    );
    r.register(
        SV_VEC,
        "fromArray",
        "(Ljdk/incubator/vector/VectorSpecies;[SI)Ljdk/incubator/vector/ShortVector;",
        sv_vec_from_array,
    );
    r.register(
        SV_VEC,
        "add",
        "(Ljdk/incubator/vector/ShortVector;)Ljdk/incubator/vector/ShortVector;",
        sv_vec_add,
    );
    r.register(
        SV_VEC,
        "sub",
        "(Ljdk/incubator/vector/ShortVector;)Ljdk/incubator/vector/ShortVector;",
        sv_vec_sub,
    );
    r.register(
        SV_VEC,
        "mul",
        "(Ljdk/incubator/vector/ShortVector;)Ljdk/incubator/vector/ShortVector;",
        sv_vec_mul,
    );
    r.register(
        SV_VEC,
        "and",
        "(Ljdk/incubator/vector/ShortVector;)Ljdk/incubator/vector/ShortVector;",
        sv_vec_and,
    );
    r.register(
        SV_VEC,
        "or",
        "(Ljdk/incubator/vector/ShortVector;)Ljdk/incubator/vector/ShortVector;",
        sv_vec_or,
    );
    r.register(
        SV_VEC,
        "xor",
        "(Ljdk/incubator/vector/ShortVector;)Ljdk/incubator/vector/ShortVector;",
        sv_vec_xor,
    );
    r.register(
        SV_VEC,
        "neg",
        "()Ljdk/incubator/vector/ShortVector;",
        sv_vec_neg,
    );
    r.register(
        SV_VEC,
        "abs",
        "()Ljdk/incubator/vector/ShortVector;",
        sv_vec_abs,
    );
    r.register(SV_VEC, "reduceLanes", "(I)S", sv_vec_reduce_lanes);
    r.register(SV_VEC, "lane", "(I)S", sv_vec_lane);
    r.register(
        SV_VEC,
        "withLane",
        "(IS)Ljdk/incubator/vector/ShortVector;",
        sv_vec_with_lane,
    );
    r.register(SV_VEC, "length", "()I", sv_vec_length);
    r.register(
        SV_VEC,
        "species",
        "()Ljdk/incubator/vector/VectorSpecies;",
        sv_vec_species,
    );
    r.set_category(__prev_cat);
}

fn register_vector_mask(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        VM,
        "fromValues",
        "(II)Ljdk/incubator/vector/VectorMask;",
        vm_from_values,
    );
    r.register(
        VM,
        "allTrue",
        "(I)Ljdk/incubator/vector/VectorMask;",
        vm_all_true,
    );
    r.register(
        VM,
        "allFalse",
        "(I)Ljdk/incubator/vector/VectorMask;",
        vm_all_false,
    );
    r.register(VM, "trueCount", "()I", vm_true_count);
    r.register(VM, "laneIsSet", "(I)Z", vm_lane_is_set);
    r.register(
        VM,
        "and",
        "(Ljdk/incubator/vector/VectorMask;)Ljdk/incubator/vector/VectorMask;",
        vm_and,
    );
    r.register(
        VM,
        "or",
        "(Ljdk/incubator/vector/VectorMask;)Ljdk/incubator/vector/VectorMask;",
        vm_or,
    );
    r.register(VM, "not", "()Ljdk/incubator/vector/VectorMask;", vm_not);
    r.register(VM, "length", "()I", vm_length);
    r.set_category(__prev_cat);
}

fn register_vector_shuffle(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        VSH,
        "fromValues",
        "(II)Ljdk/incubator/vector/VectorShuffle;",
        vsh_from_values,
    );
    r.register(
        VSH,
        "iota",
        "(I)Ljdk/incubator/vector/VectorShuffle;",
        vsh_iota,
    );
    r.register(VSH, "length", "()I", vsh_length);
    r.register(VSH, "laneSource", "(I)I", vsh_lane_source);
    r.set_category(__prev_cat);
}

fn register_vector_operators(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(VO, "opName", "(I)Ljava/lang/String;", vo_op_name);
    r.register(VO, "isAssociative", "(I)Z", vo_is_associative);
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod vector_api_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use cratonvm_native_api::test_mock::MockNativeContext;
    use cratonvm_native_api::NativeMethodRegistry;

    fn make_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_vector_api_natives(&mut r);
        r
    }

    // --- VectorSpeciesConfig tests ---

    #[test]
    fn test_species_config_int_256() {
        let cfg = VectorSpeciesConfig::new(ELEM_INT, 256);
        assert_eq!(cfg.element_type, ELEM_INT);
        assert_eq!(cfg.bit_size, 256);
        assert_eq!(cfg.lane_count, 8);
    }

    #[test]
    fn test_species_config_long_256() {
        let cfg = VectorSpeciesConfig::new(ELEM_LONG, 256);
        assert_eq!(cfg.lane_count, 4);
    }

    #[test]
    fn test_species_config_float_128() {
        let cfg = VectorSpeciesConfig::new(ELEM_FLOAT, 128);
        assert_eq!(cfg.lane_count, 4);
    }

    #[test]
    fn test_species_config_double_512() {
        let cfg = VectorSpeciesConfig::new(ELEM_DOUBLE, 512);
        assert_eq!(cfg.lane_count, 8);
    }

    #[test]
    fn test_species_config_byte_64() {
        let cfg = VectorSpeciesConfig::new(ELEM_BYTE, 64);
        assert_eq!(cfg.lane_count, 8);
    }

    #[test]
    fn test_species_config_short_128() {
        let cfg = VectorSpeciesConfig::new(ELEM_SHORT, 128);
        assert_eq!(cfg.lane_count, 8);
    }

    #[test]
    fn test_species_config_unknown_type() {
        let cfg = VectorSpeciesConfig::new(255, 256);
        assert_eq!(cfg.lane_count, 0);
    }

    #[test]
    fn test_element_size_bits_all_types() {
        assert_eq!(element_size_bits(ELEM_BYTE), 8);
        assert_eq!(element_size_bits(ELEM_SHORT), 16);
        assert_eq!(element_size_bits(ELEM_INT), 32);
        assert_eq!(element_size_bits(ELEM_LONG), 64);
        assert_eq!(element_size_bits(ELEM_FLOAT), 32);
        assert_eq!(element_size_bits(ELEM_DOUBLE), 64);
        assert_eq!(element_size_bits(99), 0);
    }

    #[test]
    fn test_get_species_config_valid_indices() {
        let c0 = get_species_config(0);
        assert_eq!(c0.element_type, ELEM_INT);
        assert_eq!(c0.bit_size, 64);
        assert_eq!(c0.lane_count, 2);

        let c2 = get_species_config(2);
        assert_eq!(c2.element_type, ELEM_INT);
        assert_eq!(c2.bit_size, 256);
        assert_eq!(c2.lane_count, 8);
    }

    #[test]
    fn test_get_species_config_fallback() {
        let cfg = get_species_config(999);
        assert_eq!(cfg.element_type, ELEM_INT);
        assert_eq!(cfg.bit_size, 256);
    }

    #[test]
    fn test_lane_count_from_species() {
        assert_eq!(lane_count_from_species(0), 2); // 64-bit Int = 2 lanes
        assert_eq!(lane_count_from_species(2), 8); // 256-bit Int = 8 lanes
        assert_eq!(lane_count_from_species(3), 16); // 512-bit Int = 16 lanes
    }

    // --- VectorOperators tests ---

    #[test]
    fn test_op_name_known() {
        assert_eq!(op_name(OP_NEG), "NEG");
        assert_eq!(op_name(OP_ADD), "ADD");
        assert_eq!(op_name(OP_MUL), "MUL");
        assert_eq!(op_name(OP_FMA), "FMA");
        assert_eq!(op_name(CMP_EQ), "EQ");
        assert_eq!(op_name(CMP_GE), "GE");
    }

    #[test]
    fn test_op_name_unknown() {
        assert_eq!(op_name(999), "UNKNOWN");
    }

    #[test]
    fn test_is_associative_true() {
        assert!(is_associative(OP_ADD));
        assert!(is_associative(OP_MUL));
        assert!(is_associative(OP_AND));
        assert!(is_associative(OP_OR));
        assert!(is_associative(OP_XOR));
        assert!(is_associative(OP_MIN));
        assert!(is_associative(OP_MAX));
    }

    #[test]
    fn test_is_associative_false() {
        assert!(!is_associative(OP_SUB));
        assert!(!is_associative(OP_DIV));
        assert!(!is_associative(OP_NEG));
        assert!(!is_associative(OP_ABS));
        assert!(!is_associative(CMP_EQ));
    }

    // --- Registration tests ---

    #[test]
    fn test_vector_support_jdk25_natives_registered() {
        let r = make_registry();
        assert!(r.find(VECTOR_SUPPORT, "registerNatives", "()I").is_some());
        assert!(r
            .find(VECTOR_SUPPORT, "getCPUFeatures", "()Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(VECTOR_SUPPORT, "getMaxLaneCount", "(Ljava/lang/Class;)I")
            .is_some());
    }

    #[test]
    fn test_float_vector_mul_reduce_uses_real_lanes() {
        let mut ctx = MockNativeContext::new();
        let species = alloc_species(&mut ctx, 10).unwrap();
        let lhs = ctx.new_array(ArrayElementType::Float, 8);
        let rhs = ctx.new_array(ArrayElementType::Float, 8);
        for i in 0..8 {
            ctx.set_array_element(lhs, i, Value::Float((i + 1) as f32));
            ctx.set_array_element(rhs, i, Value::Float(0.5));
        }

        let lhs_vec = fv_from_array(
            &mut ctx,
            &[
                Value::Object(Some(species)),
                Value::Object(Some(lhs)),
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap();
        let rhs_vec = fv_from_array(
            &mut ctx,
            &[
                Value::Object(Some(species)),
                Value::Object(Some(rhs)),
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap();
        let product = fv_mul(&mut ctx, &[lhs_vec, rhs_vec]).unwrap().unwrap();
        let reduced = fv_reduce_lanes(&mut ctx, &[product, Value::Int(RED_ADD)])
            .unwrap()
            .unwrap();

        assert_eq!(reduced.as_float(), Some(18.0));
    }

    #[test]
    fn test_vs_of_int_registered() {
        let r = make_registry();
        assert!(r
            .find(VS, "ofInt", "()Ljdk/incubator/vector/VectorSpecies;")
            .is_some());
    }

    #[test]
    fn test_vs_of_long_registered() {
        let r = make_registry();
        assert!(r
            .find(VS, "ofLong", "()Ljdk/incubator/vector/VectorSpecies;")
            .is_some());
    }

    #[test]
    fn test_vs_of_float_registered() {
        let r = make_registry();
        assert!(r
            .find(VS, "ofFloat", "()Ljdk/incubator/vector/VectorSpecies;")
            .is_some());
    }

    #[test]
    fn test_vs_of_double_registered() {
        let r = make_registry();
        assert!(r
            .find(VS, "ofDouble", "()Ljdk/incubator/vector/VectorSpecies;")
            .is_some());
    }

    #[test]
    fn test_vs_length_registered() {
        let r = make_registry();
        assert!(r.find(VS, "length", "()I").is_some());
    }

    #[test]
    fn test_vs_vector_bit_size_registered() {
        let r = make_registry();
        assert!(r.find(VS, "vectorBitSize", "()I").is_some());
    }

    #[test]
    fn test_iv_zero_registered() {
        let r = make_registry();
        assert!(r
            .find(
                IV,
                "zero",
                "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/IntVector;"
            )
            .is_some());
    }

    #[test]
    fn test_iv_broadcast_registered() {
        let r = make_registry();
        assert!(r
            .find(
                IV,
                "broadcast",
                "(Ljdk/incubator/vector/VectorSpecies;I)Ljdk/incubator/vector/IntVector;"
            )
            .is_some());
    }

    #[test]
    fn test_iv_add_registered() {
        let r = make_registry();
        assert!(r
            .find(
                IV,
                "add",
                "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;"
            )
            .is_some());
    }

    #[test]
    fn test_iv_sub_registered() {
        let r = make_registry();
        assert!(r
            .find(
                IV,
                "sub",
                "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;"
            )
            .is_some());
    }

    #[test]
    fn test_iv_mul_registered() {
        let r = make_registry();
        assert!(r
            .find(
                IV,
                "mul",
                "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;"
            )
            .is_some());
    }

    #[test]
    fn test_iv_reduce_lanes_registered() {
        let r = make_registry();
        assert!(r.find(IV, "reduceLanes", "(I)I").is_some());
    }

    #[test]
    fn test_iv_lane_registered() {
        let r = make_registry();
        assert!(r.find(IV, "lane", "(I)I").is_some());
    }

    #[test]
    fn test_iv_with_lane_registered() {
        let r = make_registry();
        assert!(r
            .find(IV, "withLane", "(II)Ljdk/incubator/vector/IntVector;")
            .is_some());
    }

    #[test]
    fn test_iv_blend_registered() {
        let r = make_registry();
        assert!(r.find(IV, "blend", "(Ljdk/incubator/vector/IntVector;Ljdk/incubator/vector/VectorMask;)Ljdk/incubator/vector/IntVector;").is_some());
    }

    #[test]
    fn test_iv_compare_registered() {
        let r = make_registry();
        assert!(r
            .find(
                IV,
                "compare",
                "(ILjdk/incubator/vector/IntVector;)Ljdk/incubator/vector/VectorMask;"
            )
            .is_some());
    }

    #[test]
    fn test_iv_rearrange_registered() {
        let r = make_registry();
        assert!(r
            .find(
                IV,
                "rearrange",
                "(Ljdk/incubator/vector/VectorShuffle;)Ljdk/incubator/vector/IntVector;"
            )
            .is_some());
    }

    #[test]
    fn test_lv_zero_registered() {
        let r = make_registry();
        assert!(r
            .find(
                LV,
                "zero",
                "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/LongVector;"
            )
            .is_some());
    }

    #[test]
    fn test_lv_add_registered() {
        let r = make_registry();
        assert!(r
            .find(
                LV,
                "add",
                "(Ljdk/incubator/vector/LongVector;)Ljdk/incubator/vector/LongVector;"
            )
            .is_some());
    }

    #[test]
    fn test_lv_reduce_lanes_registered() {
        let r = make_registry();
        assert!(r.find(LV, "reduceLanes", "(I)J").is_some());
    }

    #[test]
    fn test_fv_zero_registered() {
        let r = make_registry();
        assert!(r
            .find(
                FV,
                "zero",
                "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/FloatVector;"
            )
            .is_some());
    }

    #[test]
    fn test_fv_sqrt_registered() {
        let r = make_registry();
        assert!(r
            .find(FV, "sqrt", "()Ljdk/incubator/vector/FloatVector;")
            .is_some());
    }

    #[test]
    fn test_fv_fma_registered() {
        let r = make_registry();
        assert!(r.find(FV, "fma", "(Ljdk/incubator/vector/FloatVector;Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;").is_some());
    }

    #[test]
    fn test_fv_min_max_registered() {
        let r = make_registry();
        assert!(r
            .find(
                FV,
                "min",
                "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;"
            )
            .is_some());
        assert!(r
            .find(
                FV,
                "max",
                "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;"
            )
            .is_some());
    }

    #[test]
    fn test_dv_zero_registered() {
        let r = make_registry();
        assert!(r
            .find(
                DV,
                "zero",
                "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/DoubleVector;"
            )
            .is_some());
    }

    #[test]
    fn test_dv_fma_registered() {
        let r = make_registry();
        assert!(r.find(DV, "fma", "(Ljdk/incubator/vector/DoubleVector;Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;").is_some());
    }

    #[test]
    fn test_dv_min_max_registered() {
        let r = make_registry();
        assert!(r
            .find(
                DV,
                "min",
                "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;"
            )
            .is_some());
        assert!(r
            .find(
                DV,
                "max",
                "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;"
            )
            .is_some());
    }

    #[test]
    fn test_vm_all_true_registered() {
        let r = make_registry();
        assert!(r
            .find(VM, "allTrue", "(I)Ljdk/incubator/vector/VectorMask;")
            .is_some());
    }

    #[test]
    fn test_vm_all_false_registered() {
        let r = make_registry();
        assert!(r
            .find(VM, "allFalse", "(I)Ljdk/incubator/vector/VectorMask;")
            .is_some());
    }

    #[test]
    fn test_vm_and_registered() {
        let r = make_registry();
        assert!(r
            .find(
                VM,
                "and",
                "(Ljdk/incubator/vector/VectorMask;)Ljdk/incubator/vector/VectorMask;"
            )
            .is_some());
    }

    #[test]
    fn test_vm_not_registered() {
        let r = make_registry();
        assert!(r
            .find(VM, "not", "()Ljdk/incubator/vector/VectorMask;")
            .is_some());
    }

    #[test]
    fn test_vsh_iota_registered() {
        let r = make_registry();
        assert!(r
            .find(VSH, "iota", "(I)Ljdk/incubator/vector/VectorShuffle;")
            .is_some());
    }

    #[test]
    fn test_vsh_lane_source_registered() {
        let r = make_registry();
        assert!(r.find(VSH, "laneSource", "(I)I").is_some());
    }

    #[test]
    fn test_vo_op_name_registered() {
        let r = make_registry();
        assert!(r.find(VO, "opName", "(I)Ljava/lang/String;").is_some());
    }

    #[test]
    fn test_vo_is_associative_registered() {
        let r = make_registry();
        assert!(r.find(VO, "isAssociative", "(I)Z").is_some());
    }

    // --- Species predefined index tests ---

    #[test]
    fn test_species_long_512_lanes() {
        let cfg = get_species_config(7); // SPECIES_512 Long
        assert_eq!(cfg.element_type, ELEM_LONG);
        assert_eq!(cfg.bit_size, 512);
        assert_eq!(cfg.lane_count, 8);
    }

    #[test]
    fn test_species_float_64_lanes() {
        let cfg = get_species_config(8); // SPECIES_64 Float
        assert_eq!(cfg.element_type, ELEM_FLOAT);
        assert_eq!(cfg.bit_size, 64);
        assert_eq!(cfg.lane_count, 2);
    }

    #[test]
    fn test_species_double_128_lanes() {
        let cfg = get_species_config(13); // SPECIES_128 Double
        assert_eq!(cfg.element_type, ELEM_DOUBLE);
        assert_eq!(cfg.bit_size, 128);
        assert_eq!(cfg.lane_count, 2);
    }

    #[test]
    fn test_species_byte_256_lanes() {
        let cfg = get_species_config(18); // SPECIES_256 Byte
        assert_eq!(cfg.element_type, ELEM_BYTE);
        assert_eq!(cfg.bit_size, 256);
        assert_eq!(cfg.lane_count, 32);
    }

    #[test]
    fn test_species_short_512_lanes() {
        let cfg = get_species_config(23); // SPECIES_512 Short
        assert_eq!(cfg.element_type, ELEM_SHORT);
        assert_eq!(cfg.bit_size, 512);
        assert_eq!(cfg.lane_count, 32);
    }

    // --- Total registration count ---

    #[test]
    fn test_total_registration_count() {
        let r = make_registry();
        // Verify a sampling of all categories are present
        // VectorSpecies: 8, IntVector: 25, LongVector: 16, FloatVector: 18,
        // DoubleVector: 18, VectorMask: 9, VectorShuffle: 4, VectorOperators: 2
        // Total = 100
        assert!(r.find(VS, "elementSize", "()I").is_some());
        assert!(r.find(IV, "toArray", "()[I").is_some());
        assert!(r.find(IV, "intoArray", "([II)V").is_some());
        assert!(r
            .find(IV, "species", "()Ljdk/incubator/vector/VectorSpecies;")
            .is_some());
        assert!(r.find(IV, "length", "()I").is_some());
        assert!(r
            .find(
                IV,
                "reinterpretAsLongs",
                "()Ljdk/incubator/vector/LongVector;"
            )
            .is_some());
        assert!(r
            .find(
                LV,
                "broadcast",
                "(Ljdk/incubator/vector/VectorSpecies;J)Ljdk/incubator/vector/LongVector;"
            )
            .is_some());
        assert!(r
            .find(
                FV,
                "broadcast",
                "(Ljdk/incubator/vector/VectorSpecies;F)Ljdk/incubator/vector/FloatVector;"
            )
            .is_some());
        assert!(r
            .find(
                DV,
                "broadcast",
                "(Ljdk/incubator/vector/VectorSpecies;D)Ljdk/incubator/vector/DoubleVector;"
            )
            .is_some());
        assert!(r
            .find(VM, "fromValues", "(II)Ljdk/incubator/vector/VectorMask;")
            .is_some());
        assert!(r
            .find(
                VSH,
                "fromValues",
                "(II)Ljdk/incubator/vector/VectorShuffle;"
            )
            .is_some());
    }
}
