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

/// The switch: read once per concat SITE, at construction, never per call.
fn latin1_fast_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CONCAT_LATIN1_FAST")
}

/// CONCAT5-4 (JIT round 19 proposal wave 6, lane str6;
/// `CRATONVM_JIT_CONCAT_TLAB_RESULT`, opt-in, default OFF): read once per
/// concat SITE, at construction. When on, the general renderer's all-Latin-1
/// result comes from the thread's TLAB (refill allowed), so the inline
/// concats' TLAB-full decline refills the TLAB whatever this path answers.
pub(super) fn tlab_result_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_CONCAT_TLAB_RESULT")
}

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
    site_shape_admits(recipe, constants, arg_types) && latin1_fast_enabled()
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
    let mut out = Latin1Buf::new();
    let rendered = render_latin1(
        recipe,
        constants,
        arg_types,
        values,
        |obj, out| {
            // By CLASS first (a `StringBuilder` passes the shape reader), then
            // the LATIN1 payload; anything else declines the whole site call.
            crate::vm::is_java_lang_string(shared, obj)
                && crate::vm::append_java_string_latin1(&shared.mem.heap, obj, out)
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

/// The switch: read once per concat SITE, at construction.
fn utf16_fast_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CONCAT_UTF16_FAST")
}

/// May a site take the UTF16 path at all? The tags must match the operands
/// and the constants one for one, and every operand type must be one the
/// renderer handles or declines per call. Unlike [`site_admits`], literal
/// text may be UTF16.
pub(super) fn site_admits_utf16(
    recipe: &[u16],
    constants: &[Arc<[u16]>],
    arg_types: &[char],
) -> bool {
    site_tags_admit_utf16(recipe, constants, arg_types) && utf16_fast_enabled()
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
            crate::vm::is_java_lang_string(shared, obj)
                && crate::vm::append_java_string_units(&shared.mem.heap, obj, out)
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
    if wide {
        crate::vm::try_create_java_string_utf16_threaded(shared, thread, &out)
    } else {
        // Every unit fits a byte (a UTF16 operand holding Latin-1 text only):
        // the LATIN1 String the general renderer compresses it to.
        let bytes: Latin1Buf = out.iter().map(|&u| u as u8).collect();
        crate::vm::try_create_java_string_latin1_threaded(shared, thread, &bytes)
    }
}

#[cfg(test)]
mod tests {
    //! STRGAP19-1: the Latin-1 fast path renders what the general renderer
    //! renders (HotSpot's `String.valueOf` spellings), declines everything it
    //! does not own, and builds a LATIN1 String of exactly the result length.

    use super::*;

    fn units(s: &str) -> Arc<[u16]> {
        Arc::from(s.encode_utf16().collect::<Vec<u16>>().as_slice())
    }

    fn render(recipe: &str, constants: &[&str], types: &str, values: &[Value]) -> Option<String> {
        let recipe: Vec<u16> = recipe.encode_utf16().collect();
        let constants: Vec<Arc<[u16]>> = constants.iter().map(|c| units(c)).collect();
        let types: Vec<char> = types.chars().collect();
        let mut out = Latin1Buf::new();
        render_latin1(
            &recipe,
            &constants,
            &types,
            values,
            |_, _| false, // no heap in these tests: any live reference declines
            &mut out,
        )
        .then(|| out.iter().map(|&b| b as char).collect())
    }

    #[test]
    fn primitives_render_as_string_value_of_spells_them() {
        let got = render(
            "z=\x01 b=\x01 s=\x01 i=\x01 j=\x01 c=\x01 n=\x01",
            &[],
            "ZBSIJCL",
            &[
                Value::Int(1),
                Value::Int(-128),
                Value::Int(-32768),
                Value::Int(i32::MIN),
                Value::Long(i64::MIN),
                Value::Int(0xE9), // 'é', Latin-1
                Value::Object(None),
            ],
        );
        assert_eq!(
            got.as_deref(),
            Some("z=true b=-128 s=-32768 i=-2147483648 j=-9223372036854775808 c=\u{e9} n=null")
        );
        assert_eq!(
            render(
                "\x01|\x01|\x01",
                &[],
                "ZIJ",
                &[Value::Int(0), Value::Int(0), Value::Long(i64::MAX)]
            )
            .as_deref(),
            Some("false|0|9223372036854775807")
        );
    }

    #[test]
    fn floats_use_the_general_renderers_formatters() {
        let got = render(
            "\x01;\x01",
            &[],
            "FD",
            &[Value::Float(1.0e10), Value::Double(-0.0)],
        );
        let want = format!(
            "{};{}",
            crate::runtime::invokedynamic::format_float(1.0e10),
            crate::runtime::invokedynamic::format_double(-0.0)
        );
        assert_eq!(got.as_deref(), Some(want.as_str()));
    }

    #[test]
    fn constants_are_copied_in_recipe_order() {
        let got = render("<\x02|\x01|\x02>", &["K", "Q"], "I", &[Value::Int(7)]);
        assert_eq!(got.as_deref(), Some("<K|7|Q>"));
    }

    #[test]
    fn what_needs_utf16_or_tostring_declines() {
        // A `char` above 0xFF makes the result UTF16.
        assert_eq!(render("\x01", &[], "C", &[Value::Int(0x3A9)]), None);
        // A live reference this reader does not accept (a non-String, or a
        // UTF16 String) declines: its `toString()` belongs to the general path.
        // SAFETY: never dereferenced; the test reader declines every reference.
        let fake = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
        assert_eq!(render("\x01", &[], "L", &[Value::Object(Some(fake))]), None);
        // Operands that disagree with the recipe decline.
        assert_eq!(render("\x01\x01", &[], "I", &[Value::Int(1)]), None);
        assert_eq!(render("\x01", &[], "I", &[Value::Long(1)]), None);
    }

    #[test]
    fn site_admission_needs_latin1_literals_and_matching_tags() {
        let recipe = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert!(site_shape_admits(&recipe("v=\x01/\x01"), &[], &['I', 'L']));
        assert!(site_shape_admits(&recipe("\u{e9}\x02"), &[units("x")], &[]));
        // A literal above 0xFF: the result is always UTF16.
        assert!(!site_shape_admits(&recipe("\u{3a9}\x01"), &[], &['I']));
        assert!(!site_shape_admits(
            &recipe("\x02"),
            &[units("\u{3a9}")],
            &[]
        ));
        // Tag counts must match.
        assert!(!site_shape_admits(&recipe("\x01"), &[], &['I', 'I']));
        assert!(!site_shape_admits(&recipe("\x02"), &[], &[]));
        // The switch is off by default, and arms the same shape.
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_LATIN1_FAST", Some("0"))],
            || assert!(!site_admits(&recipe("\x01"), &[], &['I'])),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_LATIN1_FAST", Some("1"))],
            || assert!(site_admits(&recipe("\x01"), &[], &['I'])),
        );
    }

    /// End to end on a heap: a `String` operand, a `null`, an `int` and a
    /// constant; the result is a fresh LATIN1 String of exactly the result's
    /// length, and a UTF16 String operand or compact strings off decline.
    #[test]
    fn a_latin1_result_is_one_exact_size_string() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use crate::vm::{create_java_string_uninterned, read_java_string};
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "strgap19-latin1");
        let recipe: Vec<u16> = "v=\x01/\x01/\x01\x02".encode_utf16().collect();
        let constants = [units("!")];
        let types = ['I', 'L', 'L'];

        // Compact strings off (the test VM's default): declines.
        let s = create_java_string_uninterned(&shared, "s\u{e9}");
        let values = [Value::Int(-42), Value::Object(Some(s)), Value::Object(None)];
        assert!(try_concat(&shared, &mut thread, &recipe, &constants, &types, &values).is_none());

        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Compact strings over the synthetic two-slot String layout (no real
        // image produces it): the constructor refuses rather than write the
        // `hash` / `hashIsZero` slots past the object, so even an operand-free
        // render declines.
        let primitive_only = [Value::Int(1), Value::Object(None), Value::Object(None)];
        assert!(try_concat(
            &shared,
            &mut thread,
            &recipe,
            &constants,
            &types,
            &primitive_only
        )
        .is_none());

        // The JDK 9+ layout (`value`, `coder`, `hash`, `hashIsZero`), as a real
        // image resolves it.
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);
        let s = create_java_string_uninterned(&shared, "s\u{e9}");
        let values = [Value::Int(-42), Value::Object(Some(s)), Value::Object(None)];
        let got = try_concat(&shared, &mut thread, &recipe, &constants, &types, &values)
            .expect("an all-Latin-1 concat takes the fast path");
        assert_eq!(
            read_java_string(&shared.mem.heap, got).as_deref(),
            Some("v=-42/s\u{e9}/null!")
        );
        let mut bytes = Latin1Buf::new();
        assert!(crate::vm::append_java_string_latin1(
            &shared.mem.heap,
            got,
            &mut bytes
        ));
        assert_eq!(bytes.len(), "v=-42/s\u{e9}/null!".chars().count());
        assert_ne!(got.as_ptr(), s.as_ptr());

        // A UTF16 String operand declines (the general renderer answers it).
        let wide = create_java_string_uninterned(&shared, "\u{3a9}");
        let values = [
            Value::Int(1),
            Value::Object(Some(wide)),
            Value::Object(None),
        ];
        assert!(try_concat(&shared, &mut thread, &recipe, &constants, &types, &values).is_none());
    }

    /// STRGAP19-7: the interpreter's entry renders exactly what the general
    /// renderer renders, a fresh String per call, and declines a recipe whose
    /// tags disagree with the operands. (The unary boolean `"" + b` declines:
    /// see [`the_unary_boolean_concat_answers_the_interned_literal`].)
    #[test]
    fn the_interpreted_entry_matches_the_general_renderer() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use crate::vm::{create_java_string_uninterned, read_java_string};
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "strgap19-7-interp");
        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);
        let s = create_java_string_uninterned(&shared, "ab\u{e9}");
        let cases: Vec<(&str, Vec<Arc<[u16]>>, Vec<char>, Vec<Value>)> = vec![
            ("b=\x01", vec![], vec!['Z'], vec![Value::Int(1)]),
            (
                "\x01\x01",
                vec![],
                vec!['Z', 'Z'],
                vec![Value::Int(0), Value::Int(1)],
            ),
            (
                "k=\x01,\x01;\x02",
                vec![units("end")],
                vec!['J', 'L'],
                vec![Value::Long(i64::MIN), Value::Object(Some(s))],
            ),
            (
                "\x01\x01\x01",
                vec![],
                vec!['C', 'D', 'L'],
                vec![Value::Int(0xE9), Value::Double(-0.0), Value::Object(None)],
            ),
        ];
        for (recipe_text, constants, types, values) in &cases {
            let recipe: Vec<u16> = recipe_text.encode_utf16().collect();
            let fast = try_concat_interpreted(
                &shared,
                &mut thread,
                &recipe,
                constants,
                types,
                values,
                None,
            )
            .expect("an all-Latin-1 interpreted concat takes the fast path");
            let general = super::super::concat_values_to_string(
                &shared,
                &mut thread,
                &recipe,
                constants.as_slice(),
                types,
                values,
                false,
            )
            .ok()
            .expect("the general renderer answers the same concat");
            assert_eq!(
                read_java_string(&shared.mem.heap, fast),
                read_java_string(&shared.mem.heap, general),
                "recipe {recipe_text:?}"
            );
            assert_ne!(fast.as_ptr(), general.as_ptr(), "a fresh String each");
        }
        // Tags that disagree with the operands: the general renderer's
        // tolerance is not reproduced; this entry declines.
        let recipe: Vec<u16> = "\x01\x01".encode_utf16().collect();
        assert!(try_concat_interpreted(
            &shared,
            &mut thread,
            &recipe,
            &[] as &[Arc<[u16]>],
            &['I'],
            &[Value::Int(3)],
            None,
        )
        .is_none());
        // INTERP719-1: a cached `false` declines without the scan.
        let recipe: Vec<u16> = "\x01".encode_utf16().collect();
        assert!(try_concat_interpreted(
            &shared,
            &mut thread,
            &recipe,
            &[] as &[Arc<[u16]>],
            &['I'],
            &[Value::Int(3)],
            Some(false),
        )
        .is_none());
    }

    /// `r19-unary-indy-concat-returns-a-fresh-string-20261008` (wave 11, lane
    /// hoist11): `"" + b` -- recipe `\1`, no constants, one `Z` -- is the
    /// interned `"true"` / `"false"` literal (HotSpot's
    /// `String.valueOf(boolean)`), the same object every call; both fast
    /// paths decline it so the general renderer answers. `"" + i` and a
    /// boolean with literal text stay fresh Strings.
    #[test]
    fn the_unary_boolean_concat_answers_the_interned_literal() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use crate::vm::read_java_string;
        if !crate::runtime::env_cache::concat_unary_boolean_literal() {
            return; // killed by CRATONVM_CONCAT_UNARY_BOOLEAN_LITERAL=0
        }
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "hoist11-unary-z");
        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);
        let recipe: Vec<u16> = "\x01".encode_utf16().collect();
        let none: &[Arc<[u16]>] = &[];
        for (v, text) in [(1, "true"), (0, "false"), (7, "true")] {
            let values = [Value::Int(v)];
            assert!(try_concat(&shared, &mut thread, &recipe, none, &['Z'], &values).is_none());
            assert!(try_concat_interpreted(
                &shared,
                &mut thread,
                &recipe,
                none,
                &['Z'],
                &values,
                None
            )
            .is_none());
            let render = |thread: &mut JvmThread| {
                super::super::concat_values_to_string(
                    &shared,
                    thread,
                    &recipe,
                    none,
                    &['Z'],
                    &values,
                    false,
                )
                .ok()
                .expect("the general renderer answers")
            };
            let a = render(&mut thread);
            let b = render(&mut thread);
            let (pooled, _) = crate::vm::try_create_java_string_reporting(&shared, text)
                .expect("the literal pool answers");
            assert_eq!(read_java_string(&shared.mem.heap, a).as_deref(), Some(text));
            assert_eq!(a.as_ptr(), b.as_ptr(), "the same object every call");
            assert_eq!(a.as_ptr(), pooled.as_ptr(), "the interned literal");
        }
        // `"" + i` stays a fresh String (HotSpot: `Integer.toString`).
        let values = [Value::Int(1)];
        let a = super::super::concat_values_to_string(
            &shared,
            &mut thread,
            &recipe,
            none,
            &['I'],
            &values,
            false,
        )
        .ok()
        .expect("int");
        let b = super::super::concat_values_to_string(
            &shared,
            &mut thread,
            &recipe,
            none,
            &['I'],
            &values,
            false,
        )
        .ok()
        .expect("int");
        assert_ne!(a.as_ptr(), b.as_ptr());
    }

    /// INTERP719-1: the cached site's answer, parsed from the descriptor once,
    /// is the per-call scan's answer (the scan over the argument types
    /// `execute_string_concat` parses from the same descriptor) for every
    /// shape the rule distinguishes.
    #[test]
    fn the_cached_shape_is_the_per_call_scans_answer() {
        let cases: Vec<(&str, Vec<&str>, &str)> = vec![
            (
                "v=\x01/\x01",
                vec![],
                "(ILjava/lang/String;)Ljava/lang/String;",
            ),
            ("\x01", vec![], "(Z)Ljava/lang/String;"),
            ("k=\x01,\x01;\x02", vec!["end"], "(J[I)Ljava/lang/String;"),
            ("\u{3a9}=\x01", vec![], "(I)Ljava/lang/String;"),
            ("\x02\x01", vec!["\u{3a9}"], "(I)Ljava/lang/String;"),
            ("\x01\x01", vec![], "(I)Ljava/lang/String;"),
            ("\x01", vec!["extra"], "(I)Ljava/lang/String;"),
            ("\x01\x01\x01", vec![], "(CDF)Ljava/lang/String;"),
            ("", vec![], "()Ljava/lang/String;"),
        ];
        let mut admitted = 0;
        for (recipe_text, constants, descriptor) in &cases {
            let recipe: Vec<u16> = recipe_text.encode_utf16().collect();
            let constants: Vec<Arc<[u16]>> = constants.iter().map(|c| units(c)).collect();
            let mut types: smallvec::SmallVec<[char; 8]> = smallvec::SmallVec::new();
            super::super::parse_descriptor_args_into(descriptor, &mut types);
            let scanned = site_shape_admits_in(&recipe, &constants, &types);
            assert_eq!(
                site_shape_admits_for_descriptor(&recipe, &constants, descriptor),
                scanned,
                "recipe {recipe_text:?} descriptor {descriptor}"
            );
            admitted += usize::from(scanned);
        }
        // Positive and negative controls: both answers occur.
        assert!(
            admitted >= 3 && admitted < cases.len(),
            "admitted {admitted}"
        );
    }

    /// r21 concat (`CRATONVM_CONCAT_LENGTH_OVERFLOW_OOME`): the bounds are
    /// `StringConcatHelper`'s -- a LATIN1 result up to `Integer.MAX_VALUE`
    /// units, a UTF16 one up to 2^30 - 1 (`length << 1` stays an `int`), and
    /// compact strings off makes every result UTF16. The wide scan is asked
    /// only past the UTF16 bound. The message is HotSpot's, text for text.
    #[test]
    fn r21_concat_length_bounds_are_string_concat_helpers() {
        use super::super::{
            concat_length_overflows, concat_length_overflows_by, CONCAT_LENGTH_OVERFLOW_MESSAGE,
            CONCAT_MAX_LATIN1_UNITS, CONCAT_MAX_UTF16_UNITS,
        };
        assert_eq!(CONCAT_MAX_LATIN1_UNITS, 2_147_483_647);
        assert_eq!(CONCAT_MAX_UTF16_UNITS, (1 << 30) - 1);
        let never = || -> bool { panic!("the wide scan below the UTF16 bound") };
        assert!(!concat_length_overflows_by(0, false, never));
        assert!(!concat_length_overflows_by(CONCAT_MAX_UTF16_UNITS, true, never));
        // LATIN1 past 2^30 - 1 is fine; UTF16 is not.
        assert!(!concat_length_overflows_by(1 << 30, false, || false));
        assert!(concat_length_overflows_by(1 << 30, false, || true));
        assert!(concat_length_overflows_by(1 << 30, true, || false));
        assert!(!concat_length_overflows_by(CONCAT_MAX_LATIN1_UNITS, false, || false));
        assert!(concat_length_overflows_by(CONCAT_MAX_LATIN1_UNITS + 1, false, || false));
        // The slice form reads the units it is given.
        assert!(!concat_length_overflows(&[0x41, 0x3A9], false));
        assert_eq!(CONCAT_LENGTH_OVERFLOW_MESSAGE, "Overflow: String length out of range");
        let mut out = Latin1Buf::new();
        out.extend_from_slice(b"ab");
        assert!(out.len() <= CONCAT_MAX_LATIN1_UNITS);
    }

    /// CONCAT21-2: the unit renderer spells every operand as the Latin-1 one
    /// (and the general renderer) does, keeps a wide literal / `char` / a lone
    /// surrogate as one unit, and declines what needs `toString()`.
    #[test]
    fn r21_the_unit_renderer_spells_what_the_latin1_one_spells() {
        let units_of = |recipe: &str, constants: &[&str], types: &str, values: &[Value]| {
            let recipe: Vec<u16> = recipe.encode_utf16().collect();
            let constants: Vec<Arc<[u16]>> = constants.iter().map(|c| units(c)).collect();
            let types: Vec<char> = types.chars().collect();
            let mut out = Vec::new();
            render_units(&recipe, &constants, &types, values, |_, _| false, &mut out)
                .then_some(out)
        };
        let want = |s: &str| -> Option<Vec<u16>> { Some(s.encode_utf16().collect()) };
        assert_eq!(
            units_of(
                "\u{3a9}=\x01 \x01 \x01 \x01 \x01 \x01\x02",
                &["\u{4e2d}"],
                "IJCZLD",
                &[
                    Value::Int(i32::MIN),
                    Value::Long(i64::MAX),
                    Value::Int(0x3A9),
                    Value::Int(0),
                    Value::Object(None),
                    Value::Double(1e7),
                ]
            ),
            want("\u{3a9}=-2147483648 9223372036854775807 \u{3a9} false null 1.0E7\u{4e2d}")
        );
        // A lone surrogate char is one unit.
        assert_eq!(
            units_of("x\x01y", &[], "C", &[Value::Int(0xD800)]),
            Some(vec![u16::from(b'x'), 0xD800, u16::from(b'y')])
        );
        // The same text as the Latin-1 renderer where both answer.
        let values = [Value::Int(-7), Value::Float(1.5)];
        let l1 = render("a\x01b\x01", &[], "IF", &values).expect("latin1");
        assert_eq!(units_of("a\x01b\x01", &[], "IF", &values), want(&l1));
        // A live reference this test heap cannot read declines.
        // SAFETY: the reference is never dereferenced; the renderer declines it.
        let fake = unsafe { ObjectRef::from_raw(8usize as *mut u8) };
        assert_eq!(units_of("\x01", &[], "L", &[Value::Object(Some(fake))]), None);
        // Mismatched tags.
        assert!(!site_tags_admit_utf16(
            &"\x01\x01".encode_utf16().collect::<Vec<_>>(),
            &[] as &[Arc<[u16]>],
            &['I']
        ));
        assert!(site_tags_admit_utf16(
            &"\u{3a9}\x01".encode_utf16().collect::<Vec<_>>(),
            &[] as &[Arc<[u16]>],
            &['I']
        ));
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_UTF16_FAST", Some("0"))],
            || assert!(!utf16_fast_enabled()),
        );
        // R22-AB-FLIP: default ON.
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_UTF16_FAST", None)],
            || assert!(utf16_fast_enabled()),
        );
    }

    /// CONCAT21-2: a result with a unit above 0xFF is one UTF16 String from the
    /// TLAB with the general renderer's text; one whose units all fit a byte
    /// is LATIN1; compact strings off declines.
    #[test]
    fn r21_a_utf16_result_is_one_tlab_string() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use crate::vm::{create_java_string_uninterned, read_java_string_units};
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "r21-concat-utf16");
        let recipe: Vec<u16> = "<\x01|\x01|\x01>".encode_utf16().collect();
        let none: &[Arc<[u16]>] = &[];
        let types = ['L', 'C', 'I'];
        let values = [Value::Object(None), Value::Int(0x3A9), Value::Int(5)];
        // Compact strings off (the test VM's default): declines.
        assert!(try_concat_utf16(&shared, &mut thread, &recipe, none, &types, &values).is_none());
        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);
        let got = try_concat_utf16(&shared, &mut thread, &recipe, none, &types, &values)
            .expect("primitives and null take the UTF16 path");
        let want: Vec<u16> = "<null|\u{3a9}|5>".encode_utf16().collect();
        assert_eq!(read_java_string_units(&shared.mem.heap, got), Some(want));
        // A UTF16 String operand is copied unit for unit.
        let w = create_java_string_uninterned(&shared, "\u{4e2d}\u{e9}");
        let types = ['L', 'C', 'I'];
        let values = [Value::Object(Some(w)), Value::Int(u32::from(b'c') as i32), Value::Int(-1)];
        let got = try_concat_utf16(&shared, &mut thread, &recipe, none, &types, &values)
            .expect("a UTF16 String operand");
        let want: Vec<u16> = "<\u{4e2d}\u{e9}|c|-1>".encode_utf16().collect();
        assert_eq!(read_java_string_units(&shared.mem.heap, got), Some(want));
        // All units fit a byte: a LATIN1 String (the general renderer's coder).
        let values = [Value::Object(None), Value::Int(0xE9), Value::Int(1)];
        let got = try_concat_utf16(&shared, &mut thread, &recipe, none, &types, &values)
            .expect("latin1 units");
        let mut bytes = Latin1Buf::new();
        assert!(crate::vm::append_java_string_latin1(&shared.mem.heap, got, &mut bytes));
        assert_eq!(&bytes[..], b"<null|\xe9|1>");
        // `"" + b` keeps the interned literal (the general renderer).
        let unary: Vec<u16> = "\x01".encode_utf16().collect();
        if crate::runtime::env_cache::concat_unary_boolean_literal() {
            assert!(
                try_concat_utf16(&shared, &mut thread, &unary, none, &['Z'], &[Value::Int(1)])
                    .is_none()
            );
        }
    }
}
