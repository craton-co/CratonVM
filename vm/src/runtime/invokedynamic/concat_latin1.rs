// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The compiled concat bridge's exact-size Latin-1 fast path (JIT round 19,
//! lane strgap, STRGAP19-1; `CRATONVM_JIT_CONCAT_LATIN1_FAST`, default ON, `=0` off;
//! decided once per site at construction).
//!
//! # What it replaces
//!
//! Every compiled `"v=" + i + "/" + s` reaches
//! [`super::execute_jit_string_concat_raw`] through `jit_indy_bridge`, and the
//! frame-free arm there renders through [`super::concat_values_to_string`],
//! the interpreter's general renderer:
//!
//! * a pin per reference operand in `native_pin_roots` (needed there only
//!   because an operand `toString()` may run Java);
//! * a Rust heap `Vec<u16>` accumulator, plus a Rust `String` per primitive
//!   operand (`i32::to_string`) re-encoded to UTF-16;
//! * the result built by `create_string_from_units_or_oom`, i.e. a Latin-1
//!   scan of the units and TWO shared-heap allocations
//!   (`try_alloc_object_full`, `try_alloc_array_full`), never the thread's
//!   TLAB.
//!
//! # What it does
//!
//! When every operand renders without Java code (a primitive, a `null`, or a
//! `java.lang.String` whose coder is LATIN1) and every literal unit of the
//! recipe and its constants fits in a byte, the result's bytes are written
//! straight into an inline buffer, in the recipe's order, and a fresh String
//! of exactly that length is bump-allocated from the TLAB
//! ([`crate::vm::try_create_java_string_latin1_threaded`]). That is the shape
//! HotSpot's `StringConcatHelper` builds (`mix` the length and coder, one
//! exact-size `byte[]`, `prepend` each operand).
//!
//! Anything else -- an operand needing `toString()`, a UTF16 String or `char`,
//! compact strings off, an allocation the TLAB and the no-GC heap path cannot
//! serve -- returns `None` BEFORE any allocation or with only unreachable
//! garbage allocated, and the caller runs the general renderer, which answers
//! exactly as it did before this switch. No Java code runs on this path, so a
//! decline never repeats a side effect.
//!
//! # GC
//!
//! Every operand is read into the buffer before the first allocation, and
//! nothing between the bridge's decode and that read allocates or polls (the
//! general renderer relies on the same window for its pins). The two
//! allocations perform no collection; the only references held across them
//! are the two fresh objects themselves.

use std::sync::Arc;

use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;

use super::{TAG_ARG_UNIT, TAG_CONST_UNIT};

/// Inline capacity of the result buffer. Longer results spill to the heap
/// (still one allocation fewer than the general renderer's `Vec<u16>`).
pub(super) type Latin1Buf = smallvec::SmallVec<[u8; 128]>;



/// CONCAT5-4 (JIT round 19 proposal wave 6, lane str6;
/// `CRATONVM_JIT_CONCAT_TLAB_RESULT`, opt-in, default OFF): read once per
/// concat SITE, at construction. When on, the general renderer's all-Latin-1
/// result comes from the thread's TLAB (refill allowed), so the inline
/// concats' TLAB-full decline refills the TLAB whatever this path answers.
pub(super) fn tlab_result_enabled() -> bool {
    false}

/// May a site with this recipe, these constants and these operand types take
/// the fast path at all? Decided at site construction:
///
/// * the switch is on;
/// * every recipe unit and every constant unit fits in a byte (the literal
///   text is Latin-1), so only operands can force the general renderer;
/// * the recipe's argument and constant tags match the operands and the
///   constants one for one (the general renderer tolerates a mismatch by
///   skipping; this path does not try to reproduce that);
/// * every operand type is one this path can render or decline per call.
pub(super) fn site_admits(recipe: &[u16], constants: &[Arc<[u16]>], arg_types: &[char]) -> bool {
    site_shape_admits(recipe, constants, arg_types)
}

/// [`site_admits`] without the switch, for the unit tests.
fn site_shape_admits(recipe: &[u16], constants: &[Arc<[u16]>], arg_types: &[char]) -> bool {
    site_shape_admits_in(recipe, constants, arg_types)
}

/// [`site_shape_admits`] for any constant representation: the compiled
/// bridge's site holds `Arc<[u16]>`s, the interpreter's call site whatever
/// its cache or bootstrap path hands [`super::execute_string_concat`].
fn site_shape_admits_in<S: AsRef<[u16]>>(
    recipe: &[u16],
    constants: &[S],
    arg_types: &[char],
) -> bool {
    let arg_tags = recipe.iter().filter(|&&u| u == TAG_ARG_UNIT).count();
    let const_tags = recipe.iter().filter(|&&u| u == TAG_CONST_UNIT).count();
    arg_tags == arg_types.len()
        && const_tags == constants.len()
        && recipe.iter().all(|&u| u <= 0xFF)
        && constants
            .iter()
            .all(|c| c.as_ref().iter().all(|&u| u <= 0xFF))
        && arg_types
            .iter()
            .all(|t| matches!(t, 'Z' | 'B' | 'C' | 'S' | 'I' | 'J' | 'F' | 'D' | 'L' | '['))
}

/// INTERP719-1 (JIT round 19 proposal wave 8, lane str8): the shape rule
/// [`site_shape_admits_in`] over a descriptor's argument types, for the
/// interpreter's cached `ResolvedCallSite::StringConcat` (decided once at
/// caching). The argument types are parsed exactly as
/// [`super::execute_string_concat`] parses them per call, so the cached answer
/// is the per-call scan's answer for that site.
pub(super) fn site_shape_admits_for_descriptor<S: AsRef<[u16]>>(
    recipe: &[u16],
    constants: &[S],
    target_descriptor: &str,
) -> bool {
    let mut arg_types: smallvec::SmallVec<[char; 8]> = smallvec::SmallVec::new();
    super::parse_descriptor_args_into(target_descriptor, &mut arg_types);
    site_shape_admits_in(recipe, constants, &arg_types)
}

/// Append the decimal text of `v` exactly as `Long.toString(long)` /
/// `Integer.toString(int)` spell it (a leading `-` for a negative value; the
/// magnitude is unsigned, so `MIN_VALUE` needs no special case).
fn push_decimal(out: &mut Latin1Buf, v: i64) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut m = v.unsigned_abs();
    loop {
        at -= 1;
        digits[at] = b'0' + (m % 10) as u8;
        m /= 10;
        if m == 0 {
            break;
        }
    }
    if v < 0 {
        out.push(b'-');
    }
    out.extend_from_slice(&digits[at..]);
}

/// Render the recipe with `values` into `out` as Latin-1 bytes. `false` (with
/// `out` in an unspecified state) means "not this path": an operand needs
/// `toString()`, is not Latin-1, or the operands disagree with the recipe.
///
/// `read_string` appends a reference operand's bytes and answers `true` only
/// for a `java.lang.String` with a LATIN1 payload. It is a parameter so the
/// rendering itself is testable without a heap.
pub(super) fn render_latin1<S: AsRef<[u16]>>(
    recipe: &[u16],
    constants: &[S],
    arg_types: &[char],
    values: &[Value],
    mut read_string: impl FnMut(ObjectRef, &mut Latin1Buf) -> bool,
    out: &mut Latin1Buf,
) -> bool {
    if values.len() != arg_types.len() {
        return false;
    }
    let mut arg_idx = 0usize;
    let mut const_idx = 0usize;
    for &unit in recipe {
        if unit == TAG_ARG_UNIT {
            let (Some(&value), Some(&ty)) = (values.get(arg_idx), arg_types.get(arg_idx)) else {
                return false;
            };
            arg_idx += 1;
            match (value, ty) {
                // `String.valueOf(boolean)`: the general renderer's test is
                // `v != 0` too.
                (Value::Int(v), 'Z') => {
                    out.extend_from_slice(if v != 0 { b"true" } else { b"false" })
                }
                // A `char` is one code unit; above 0xFF the result is UTF16.
                (Value::Int(v), 'C') => {
                    let u = v as u16;
                    if u > 0xFF {
                        return false;
                    }
                    out.push(u as u8);
                }
                (Value::Int(v), 'B' | 'S' | 'I') => push_decimal(out, i64::from(v)),
                (Value::Long(v), 'J') => push_decimal(out, v),
                // The general renderer's own formatters, so the two paths
                // cannot disagree; their text is ASCII.
                (Value::Float(v), 'F') => out.extend_from_slice(super::format_float(v).as_bytes()),
                (Value::Double(v), 'D') => {
                    out.extend_from_slice(super::format_double(v).as_bytes())
                }
                // `StringConcatHelper.stringOf(null)` is "null".
                (Value::Object(None), 'L' | '[') => out.extend_from_slice(b"null"),
                (Value::Object(Some(obj)), 'L' | '[') => {
                    if !read_string(obj, out) {
                        return false;
                    }
                }
                _ => return false,
            }
        } else if unit == TAG_CONST_UNIT {
            let Some(c) = constants.get(const_idx) else {
                return false;
            };
            const_idx += 1;
            // Admitted at site construction: every unit fits in a byte.
            for &u in c.as_ref().iter() {
                if u > 0xFF {
                    return false;
                }
                out.push(u as u8);
            }
        } else {
            if unit > 0xFF {
                return false;
            }
            out.push(unit as u8);
        }
    }
    arg_idx == values.len() && const_idx == constants.len()
}

/// The fast path proper: `Some(result)` or `None` for "run the general
/// renderer". See the module doc for the contract.
pub(super) fn try_concat<S: AsRef<[u16]>>(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    recipe: &[u16],
    constants: &[S],
    arg_types: &[char],
    values: &[Value],
) -> Option<ObjectRef> {
    // A LATIN1 result only while LATIN1 Strings may exist (W9VER: `-XX:-CompactStrings`).
    if !shared.classes.latin1_coder_allowed() {
        return None;
    }
    // `r19-unary-indy-concat-returns-a-fresh-string-20261008` (JIT round 19
    // wave 11, lane hoist11): `"" + b` answers the interned literal, which
    // the general renderer builds; this path makes fresh Strings only, so it
    // declines that one shape (before any allocation).
    if super::unary_boolean_concat_operand(recipe, constants.len(), arg_types, values).is_some() {
        return None;
    }
    // `"" + s` / `s + ""` on two non-null `java.lang.String`s: return the
    // non-empty operand directly without allocating.
    if constants.is_empty()
        && recipe == [TAG_ARG_UNIT, TAG_ARG_UNIT]
        && arg_types == ['L', 'L']
    {
        if let [Value::Object(Some(a)), Value::Object(Some(b))] = values {
            if crate::vm::is_java_lang_string(shared, *a)
                && crate::vm::is_java_lang_string(shared, *b)
            {
                if crate::vm::java_string_is_empty(&shared.mem.heap, *a) {
                    return Some(shared.mem.heap.load_and_forward(*b));
                }
                if crate::vm::java_string_is_empty(&shared.mem.heap, *b) {
                    return Some(shared.mem.heap.load_and_forward(*a));
                }
            }
        }
    }
    let mut out = Latin1Buf::new();
    let rendered = render_latin1(
        recipe,
        constants,
        arg_types,
        values,
        |obj, out| {
            let obj = shared.mem.heap.load_and_forward(obj);
            // By CLASS first (a `StringBuilder` passes the shape reader), then
            // the LATIN1 payload or final boxed primitive; anything else declines.
            // CONCAT21 review: an operand that would carry the result past
            // the bound is not copied (a transient of up to 2 GiB otherwise);
            // the general renderer raises the overflow.
            if crate::vm::is_java_lang_string(shared, obj) {
                return crate::vm::append_java_string_latin1_bounded(
                    &shared.mem.heap,
                    obj,
                    out,
                    super::CONCAT_MAX_LATIN1_UNITS,
                );
            }
            append_boxed_primitive_latin1(shared, obj, out)
        },
        &mut out,
    );
    if !rendered {
        return None;
    }
    // r21 concat: a result past `StringConcatHelper`'s bound is the general
    // renderer's `OutOfMemoryError("Overflow: String length out of range")`
    // (`CRATONVM_CONCAT_LENGTH_OVERFLOW_OOME`); this path never builds it.
    // Nothing ran Java here, so the decline repeats no side effect.
    if out.len() > super::CONCAT_MAX_LATIN1_UNITS {
        return None;
    }
    crate::vm::try_create_java_string_latin1_threaded(shared, thread, &out)
}

/// STRGAP19-7 (JIT round 19 proposal wave 7, lane interp7;
/// `CRATONVM_INTERP_CONCAT_LATIN1_FAST`, default OFF, asked by the caller):
/// [`try_concat`] for the interpreter's concat
/// ([`super::execute_string_concat`]), which has no construction-time site
/// record, so the shape rule [`site_admits`] applies once per site is asked
/// here per call (a scan of the recipe and the constants; no allocation).
///
/// Same contract as the bridge's path: `Some` is the String the general
/// renderer would build (same text, LATIN1, fresh, `hash` 0; the unary
/// `"" + b` case, which the general renderer answers with the interned
/// literal since wave 11 -- `r19-unary-indy-concat-returns-a-fresh-string-20261008`
/// -- declines in [`try_concat`]), and `None` -- before any Java code ran and
/// with nothing reachable allocated -- sends the call to the general renderer,
/// whose output, `toString()` calls and exceptions are then unchanged.
///
/// `cached_shape` (INTERP719-1) is the cached call site's construction-time
/// answer of the same rule ([`site_shape_admits_for_descriptor`]); `None`
/// asks the scan here.
pub(super) fn try_concat_interpreted<S: AsRef<[u16]>>(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    recipe: &[u16],
    constants: &[S],
    arg_types: &[char],
    values: &[Value],
    cached_shape: Option<bool>,
) -> Option<ObjectRef> {
    let admitted =
        cached_shape.unwrap_or_else(|| site_shape_admits_in(recipe, constants, arg_types));
    if !admitted {
        return None;
    }
    try_concat(shared, thread, recipe, constants, arg_types, values)
}

// ---- CONCAT21-2: the UTF16 sibling ------------------------------------------
//
// JIT round 21 wave 5, lane concat (`CRATONVM_JIT_CONCAT_UTF16_FAST`, opt-in,
// default OFF, decided once per site). The Latin-1 path above declines every
// result with a unit above 0xFF -- a UTF16 `String` operand, a `char` above
// 0xFF, a wide literal -- and the general renderer then pins every reference,
// formats each primitive into a Rust `String`, and allocates the result on the
// shared heap. Here the same operands (primitives, `null`, `java.lang.String`
// of either coder: nothing that runs Java code) are rendered as units into
// one buffer, and the result is built from the thread's TLAB: UTF16 when a
// unit is above 0xFF, LATIN1 otherwise -- the coder the general renderer's
// `create_string_from_units_or_oom` picks for the same units. Anything else
// declines BEFORE any allocation, with no Java code run.



/// May a site take the UTF16 path at all? The tags must match the operands
/// and the constants one for one, and every operand type must be one the
/// renderer handles or declines per call. Unlike [`site_admits`], literal
/// text may be UTF16.
pub(super) fn site_admits_utf16(
    recipe: &[u16],
    constants: &[Arc<[u16]>],
    arg_types: &[char],
) -> bool {
    site_tags_admit_utf16(recipe, constants, arg_types)
}

/// [`site_admits_utf16`] without the switch, for the unit tests.
fn site_tags_admit_utf16<S: AsRef<[u16]>>(
    recipe: &[u16],
    constants: &[S],
    arg_types: &[char],
) -> bool {
    let arg_tags = recipe.iter().filter(|&&u| u == TAG_ARG_UNIT).count();
    let const_tags = recipe.iter().filter(|&&u| u == TAG_CONST_UNIT).count();
    arg_tags == arg_types.len()
        && const_tags == constants.len()
        && arg_types
            .iter()
            .all(|t| matches!(t, 'Z' | 'B' | 'C' | 'S' | 'I' | 'J' | 'F' | 'D' | 'L' | '['))
}

/// Append the ASCII `text` as units.
fn push_ascii_units(out: &mut Vec<u16>, text: &[u8]) {
    out.extend(text.iter().map(|&b| u16::from(b)));
}

/// Render the recipe with `values` into `out` as UTF-16 units. `false` means
/// "not this path" (an operand needs `toString()`, or the operands disagree
/// with the recipe). `read_string` appends a reference operand's units and
/// answers `true` only for a `java.lang.String`; a parameter so the rendering
/// is testable without a heap. Spellings are [`render_latin1`]'s.
pub(super) fn render_units<S: AsRef<[u16]>>(
    recipe: &[u16],
    constants: &[S],
    arg_types: &[char],
    values: &[Value],
    mut read_string: impl FnMut(ObjectRef, &mut Vec<u16>) -> bool,
    out: &mut Vec<u16>,
) -> bool {
    if values.len() != arg_types.len() {
        return false;
    }
    let mut arg_idx = 0usize;
    let mut const_idx = 0usize;
    for &unit in recipe {
        if unit == TAG_ARG_UNIT {
            let (Some(&value), Some(&ty)) = (values.get(arg_idx), arg_types.get(arg_idx)) else {
                return false;
            };
            arg_idx += 1;
            match (value, ty) {
                (Value::Int(v), 'Z') => {
                    push_ascii_units(out, if v != 0 { b"true" } else { b"false" })
                }
                // One code unit, whatever it is (a lone surrogate included).
                (Value::Int(v), 'C') => out.push(v as u16),
                (Value::Int(v), 'B' | 'S' | 'I') => {
                    let mut b = Latin1Buf::new();
                    push_decimal(&mut b, i64::from(v));
                    push_ascii_units(out, &b);
                }
                (Value::Long(v), 'J') => {
                    let mut b = Latin1Buf::new();
                    push_decimal(&mut b, v);
                    push_ascii_units(out, &b);
                }
                (Value::Float(v), 'F') => push_ascii_units(out, super::format_float(v).as_bytes()),
                (Value::Double(v), 'D') => {
                    push_ascii_units(out, super::format_double(v).as_bytes())
                }
                (Value::Object(None), 'L' | '[') => push_ascii_units(out, b"null"),
                (Value::Object(Some(obj)), 'L' | '[') => {
                    if !read_string(obj, out) {
                        return false;
                    }
                }
                _ => return false,
            }
        } else if unit == TAG_CONST_UNIT {
            let Some(c) = constants.get(const_idx) else {
                return false;
            };
            const_idx += 1;
            out.extend_from_slice(c.as_ref());
        } else {
            out.push(unit);
        }
        // Past the bound: the general renderer's overflow OOME, not this path.
        if out.len() > super::CONCAT_MAX_LATIN1_UNITS {
            return false;
        }
    }
    arg_idx == values.len() && const_idx == constants.len()
}

/// Would copying `obj`'s units after `have` rendered units pass
/// `StringConcatHelper`'s Latin-1 length bound? (`java_string_unit_count` is
/// `None` for a malformed String, which the copy itself then declines.)
fn string_would_pass_the_bound(shared: &SharedVm, obj: ObjectRef, have: usize) -> bool {
    crate::vm::java_string_unit_count(&shared.mem.heap, obj)
        .is_some_and(|n| have.saturating_add(n) > super::CONCAT_MAX_LATIN1_UNITS)
}

/// The UTF16 path proper: `Some(result)` or `None` for "run the general
/// renderer". See the section comment for the contract.
pub(super) fn try_concat_utf16<S: AsRef<[u16]>>(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    recipe: &[u16],
    constants: &[S],
    arg_types: &[char],
    values: &[Value],
) -> Option<ObjectRef> {
    if !shared
        .classes
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return None;
    }
    // `"" + b` is the interned literal (the general renderer's answer).
    if super::unary_boolean_concat_operand(recipe, constants.len(), arg_types, values).is_some() {
        return None;
    }
    let mut out: Vec<u16> = Vec::with_capacity(recipe.len() + 16 * values.len());
    let rendered = render_units(
        recipe,
        constants,
        arg_types,
        values,
        |obj, out| {
            let obj = shared.mem.heap.load_and_forward(obj);
            if crate::vm::is_java_lang_string(shared, obj) {
                return !string_would_pass_the_bound(shared, obj, out.len())
                    && crate::vm::append_java_string_units(&shared.mem.heap, obj, out);
            }
            append_boxed_primitive_units(shared, obj, out)
        },
        &mut out,
    );
    if !rendered {
        return None;
    }
    let wide = out.iter().any(|&u| u > 0xFF);
    // HotSpot's UTF16 bound (2^30 - 1 units): the general renderer's OOME.
    if super::concat_length_overflows_by(out.len(), false, || wide) {
        return None;
    }
    // CONCAT21 review: `-XX:-CompactStrings` (`latin1_coder_allowed` false, the
    // layout bit `compact_strings` still true) makes every String UTF16: the
    // JDK's `coder()` reads `COMPACT_STRINGS ? coder : UTF16`, so a LATIN1
    // payload built here would be read as UTF16 units. The Latin-1 path above
    // declines that policy for the same reason.
    if wide || !shared.classes.latin1_coder_allowed() {
        crate::vm::try_create_java_string_utf16_threaded(shared, thread, &out)
    } else {
        // Every unit fits a byte (a UTF16 operand holding Latin-1 text only):
        // the LATIN1 String the general renderer compresses it to.
        let bytes: Latin1Buf = out.iter().map(|&u| u as u8).collect();
        crate::vm::try_create_java_string_latin1_threaded(shared, thread, &bytes)
    }
}


enum BoxedPrimitive {
    Int(i32),
    Long(i64),
    Char(u16),
    Bool(bool),
}

/// Read `value` from final bootstrap boxed wrappers (`Integer`, `Long`,
/// `Character`, `Boolean`) so TLAB-backed bridge fallback formats them without
/// leaving `thread.tlab` pinned at full.
fn read_boxed_primitive(shared: &SharedVm, obj: ObjectRef) -> Option<BoxedPrimitive> {
    if shared.mem.heap.kind_of(obj) != crate::memory::heap::ObjectKind::Object {
        return None;
    }
    let class_id = shared.mem.heap.class_id_of(obj);
    let (tag, slot) = {
        let cm = shared.classes.class_manager.read();
        let cls = cm.get_class(class_id)?;
        let tag = match cls.name.as_ref() {
            "java/lang/Integer" => 0u8,
            "java/lang/Long" => 1,
            "java/lang/Character" => 2,
            "java/lang/Boolean" => 3,
            _ => return None,
        };
        let slot = crate::vm::vm_exec::resolve_field_index_in_hierarchy(
            class_id,
            "value",
            &cm.class_store,
        )?;
        (tag, slot)
    };
    match (tag, shared.mem.heap.get_field(obj, slot)) {
        (0, Value::Int(v)) => Some(BoxedPrimitive::Int(v)),
        (1, Value::Long(v)) => Some(BoxedPrimitive::Long(v)),
        (2, Value::Int(v)) => Some(BoxedPrimitive::Char(v as u16)),
        (3, Value::Int(v)) => Some(BoxedPrimitive::Bool(v != 0)),
        _ => None,
    }
}

fn append_boxed_primitive_latin1(shared: &SharedVm, obj: ObjectRef, out: &mut Latin1Buf) -> bool {
    let Some(prim) = read_boxed_primitive(shared, obj) else {
        return false;
    };
    match prim {
        BoxedPrimitive::Int(v) => push_decimal(out, i64::from(v)),
        BoxedPrimitive::Long(v) => push_decimal(out, v),
        BoxedPrimitive::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
        BoxedPrimitive::Char(c) if c <= 0xFF => out.push(c as u8),
        BoxedPrimitive::Char(_) => return false,
    }
    true
}

fn append_boxed_primitive_units(shared: &SharedVm, obj: ObjectRef, out: &mut Vec<u16>) -> bool {
    let Some(prim) = read_boxed_primitive(shared, obj) else {
        return false;
    };
    match prim {
        BoxedPrimitive::Char(c) => out.push(c),
        BoxedPrimitive::Bool(b) => {
            let mut buf = Latin1Buf::new();
            buf.extend_from_slice(if b { b"true" } else { b"false" });
            out.extend(buf.into_iter().map(u16::from));
        }
        BoxedPrimitive::Int(v) => {
            let mut buf = Latin1Buf::new();
            push_decimal(&mut buf, i64::from(v));
            out.extend(buf.into_iter().map(u16::from));
        }
        BoxedPrimitive::Long(v) => {
            let mut buf = Latin1Buf::new();
            push_decimal(&mut buf, v);
            out.extend(buf.into_iter().map(u16::from));
        }
    }
    true
}

#[cfg(test)]
mod tests;
