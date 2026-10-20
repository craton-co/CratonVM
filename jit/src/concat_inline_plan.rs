// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The compile-time plan of an inline `StringConcatFactory` concatenation
//! (JIT round 19 proposal wave 4, lane strgap4, STRGAP19-3:
//! `OptimizeStringConcat` for indy; `CRATONVM_JIT_CONCAT_INLINE`, default ON since the round-19 close, `=0` off).
//!
//! # What it is
//!
//! A bridged concat site (`invokedynamic` whose bootstrap is
//! `StringConcatFactory`) calls the VM's `jit_indy_bridge` per evaluation. For
//! the common shapes -- `String`, `int`/`short`/`byte`, `long`, `char` and
//! `boolean` operands around Latin-1 literal text -- the single-pass backend
//! can build the result itself: one length pass over the staged operands, one
//! TLAB bump for the exact-size `byte[]` and the `String`, one fill pass. That
//! is the shape HotSpot's C2 reaches by inlining the `StringConcatHelper`
//! `mix` / `prepend` chain.
//!
//! The VM builds this plan from the site's recipe, constants and descriptor
//! (it owns them); this crate reads it through
//! `DirectHelperTable::concat_inline_plan` at compile time. The plan is pure
//! data and VM-independent, so it lives in the process-lived site allocation.
//!
//! # What it admits
//!
//! [`JitConcatInlinePlan::from_recipe`] answers `None` -- the site keeps the
//! bridge call, byte for byte -- for anything this plan cannot render without
//! running Java code: an operand typed other than the six kinds above (an `Object`, a `float`,
//! a `double`, an array: their rendering is `toString()` or the VM's float
//! formatter), recipe tags that disagree with the operands or constants, and
//! sites past the code-size bounds. At run time every operand that is not
//! Latin-1 (a UTF16 `String`, a `char` above `0xFF`), compact strings off,
//! an empty result, a result over the inline allocation cap, a full TLAB or a
//! collector premise that fails declines to the same bridge call, with every
//! operand still in its staged slot.
//!
//! A literal unit above `0xFF` used to answer `None` as well. Since JIT round
//! 19 proposal wave 7 (lane str7, STRGAP4-4) it answers a
//! [`JitConcatInlinePlan::wide`] plan, whose result is always UTF16; the
//! compilers lower it only under `CRATONVM_JIT_CONCAT_UTF16` and otherwise
//! refuse it, which keeps the bridge call exactly as before.

/// The recipe's argument tag (`StringConcatFactory`'s `\1`).
pub const CONCAT_TAG_ARG: u16 = 1;
/// The recipe's constant tag (`StringConcatFactory`'s `\2`).
pub const CONCAT_TAG_CONST: u16 = 2;

/// Most operands one inline site renders (code size: each `String` operand
/// is ~150 bytes of machine code across the two passes).
pub const CONCAT_INLINE_MAX_ARGS: usize = 16;
/// Most literal bytes one inline site writes (code size: immediate stores).
pub const CONCAT_INLINE_MAX_LITERAL: usize = 512;

/// How one operand is rendered, from its descriptor type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConcatArgKind {
    /// `Ljava/lang/String;`: its LATIN1 bytes, or `"null"`.
    String,
    /// `I`, `S`, `B`: `Integer.toString(int)`.
    Int,
    /// `J`: `Long.toString(long)`.
    Long,
    /// `C`: the one code unit, when it fits a byte.
    Char,
    /// `Z`: `"true"` / `"false"` (the slot's low 32 bits `!= 0`).
    Boolean,
    /// `F` (JIT round 21 wave 5, lane concat, CONCAT21-3; the compilers admit
    /// it only under `CRATONVM_JIT_CONCAT_FP_INLINE`): `Float.toString(float)`
    /// of the slot's low 32 bits, rendered by the VM's shared formatter
    /// (`cratonvm_types::java_float_to_string`, the one the bridge uses).
    Float,
    /// `D` (CONCAT21-3, same switch): `Double.toString(double)` of the slot.
    Double,
}

/// One piece of the result, in recipe order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConcatSegment {
    /// Literal text (recipe units and constants, merged), every byte Latin-1.
    Literal(Vec<u8>),
    /// Literal text of a [`JitConcatInlinePlan::wide`] plan, as UTF-16 code
    /// units (JIT round 19 proposal wave 7, lane str7, STRGAP4-4). Only a wide
    /// plan carries it, and a wide plan carries no [`Self::Literal`].
    WideLiteral(Vec<u16>),
    /// Operand `index` (0 = the first descriptor parameter).
    Arg { index: usize, kind: ConcatArgKind },
}

/// The plan of one inline concat site. See the module doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitConcatInlinePlan {
    pub segments: Vec<ConcatSegment>,
    /// Code units of literal text over all segments (bytes, for a Latin-1
    /// plan).
    pub literal_len: usize,
    /// Operands of the site (one staged word each, the bridge's ABI).
    pub arg_count: usize,
    /// JIT round 19 proposal wave 7, lane str7 (STRGAP4-4; the compilers read
    /// `CRATONVM_JIT_CONCAT_UTF16`, default OFF): the literal text holds a
    /// unit above `0xFF`, so EVERY result of the site is UTF16 (HotSpot's
    /// `StringConcatHelper` ORs the literal's coder into the result's, and a
    /// non-Latin-1 char can never be compressed away). Its literals are
    /// [`ConcatSegment::WideLiteral`]s, a `char` operand of any value renders
    /// inline, and a UTF16 `String` operand is copied rather than declined. A
    /// compiler without the switch refuses a wide plan, which is exactly the
    /// `None` this site answered before.
    pub wide: bool,
    /// JIT round 21 wave 4, lane concat (CONCAT21-1; the compilers read
    /// `CRATONVM_JIT_CONCAT_CODER_PROMOTE`, default OFF): the UTF16 twin of a
    /// LATIN1 plan -- the same segments with every literal widened to units,
    /// `wide: true` -- for a site that has an operand which can force a UTF16
    /// result at run time (a `String` or a `char`). The LATIN1 passes' UTF16
    /// edges (a UTF16 `String` operand, a `char` above `0xFF`) then land on
    /// the twin's passes instead of the bridge: HotSpot's
    /// `StringConcatHelper` promotes the coder mid-chain the same way (`mix`
    /// ORs each operand's coder in), so the answer is the one the bridge
    /// builds. `None` for a wide plan, and for a plan whose operands are all
    /// `int` / `long` / `boolean` (never UTF16). Owned by the plan, so a
    /// `'static` plan has a `'static` twin.
    pub wide_twin: Option<Box<JitConcatInlinePlan>>,
}

/// The operand kind of one descriptor parameter, or `None` for a type the
/// inline lowering does not render.
fn arg_kind(param: &str) -> Option<ConcatArgKind> {
    match param {
        "Ljava/lang/String;" => Some(ConcatArgKind::String),
        "I" | "S" | "B" => Some(ConcatArgKind::Int),
        "J" => Some(ConcatArgKind::Long),
        "C" => Some(ConcatArgKind::Char),
        "Z" => Some(ConcatArgKind::Boolean),
        "F" => Some(ConcatArgKind::Float),
        "D" => Some(ConcatArgKind::Double),
        _ => None,
    }
}

/// The parameter type strings of a method descriptor, or `None` when it is
/// malformed.
fn descriptor_params(descriptor: &str) -> Option<Vec<&str>> {
    let inner = descriptor.strip_prefix('(')?;
    let close = inner.find(')')?;
    let params = &inner[..close];
    let bytes = params.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let start = i;
        while bytes.get(i) == Some(&b'[') {
            i += 1;
        }
        match bytes.get(i)? {
            b'L' => {
                let semi = params[i..].find(';')?;
                i += semi + 1;
            }
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => i += 1,
            _ => return None,
        }
        out.push(&params[start..i]);
    }
    Some(out)
}

impl JitConcatInlinePlan {
    /// The plan of a site with this recipe, these constants and this
    /// descriptor, or `None` (the site keeps its bridge call). See the module
    /// doc for the admission.
    pub fn from_recipe(recipe: &[u16], constants: &[&[u16]], descriptor: &str) -> Option<Self> {
        let params = descriptor_params(descriptor)?;
        // The result is a reference.
        let ret = &descriptor[descriptor.find(')')? + 1..];
        if !ret.starts_with('L') {
            return None;
        }
        if params.len() > CONCAT_INLINE_MAX_ARGS {
            return None;
        }
        let kinds: Vec<ConcatArgKind> = params
            .iter()
            .map(|p| arg_kind(p))
            .collect::<Option<Vec<_>>>()?;
        // Literal text is gathered as UTF-16 units; a unit above 0xFF makes the
        // plan wide (STRGAP4-4), else every literal narrows to its bytes.
        let mut wide_segments: Vec<ConcatSegment> = Vec::new();
        let mut literal: Vec<u16> = Vec::new();
        let mut literal_len = 0usize;
        let mut wide = false;
        let mut arg_idx = 0usize;
        let mut const_idx = 0usize;
        for &unit in recipe {
            if unit == CONCAT_TAG_ARG {
                let kind = *kinds.get(arg_idx)?;
                if !literal.is_empty() {
                    literal_len += literal.len();
                    wide_segments.push(ConcatSegment::WideLiteral(std::mem::take(&mut literal)));
                }
                wide_segments.push(ConcatSegment::Arg {
                    index: arg_idx,
                    kind,
                });
                arg_idx += 1;
            } else if unit == CONCAT_TAG_CONST {
                let c = constants.get(const_idx)?;
                const_idx += 1;
                wide |= c.iter().any(|&u| u > 0xFF);
                literal.extend_from_slice(c);
            } else {
                wide |= unit > 0xFF;
                literal.push(unit);
            }
        }
        if !literal.is_empty() {
            literal_len += literal.len();
            wide_segments.push(ConcatSegment::WideLiteral(literal));
        }
        // Tags must match the operands and the constants one for one (the
        // general renderer tolerates a mismatch by skipping; this does not).
        if arg_idx != kinds.len() || const_idx != constants.len() {
            return None;
        }
        if literal_len > CONCAT_INLINE_MAX_LITERAL {
            return None;
        }
        // CONCAT21-1: a LATIN1 plan with an operand that can be UTF16 at run
        // time keeps its wide segments as the twin.
        let can_promote = !wide
            && kinds
                .iter()
                .any(|k| matches!(k, ConcatArgKind::String | ConcatArgKind::Char));
        let wide_twin = can_promote.then(|| {
            Box::new(Self {
                segments: wide_segments.clone(),
                literal_len,
                arg_count: kinds.len(),
                wide: true,
                wide_twin: None,
            })
        });
        let segments = if wide {
            wide_segments
        } else {
            wide_segments
                .into_iter()
                .map(|seg| match seg {
                    // Cast-free narrowing: `wide` is false, so every unit fits.
                    ConcatSegment::WideLiteral(units) => ConcatSegment::Literal(
                        units
                            .iter()
                            .map(|&u| u8::try_from(u).unwrap_or(0))
                            .collect(),
                    ),
                    other => other,
                })
                .collect()
        };
        Some(Self {
            segments,
            literal_len,
            arg_count: kinds.len(),
            wide,
            wide_twin,
        })
    }

    /// CONCAT21-3: does the plan render a `float` / `double` operand? A
    /// compiler without `CRATONVM_JIT_CONCAT_FP_INLINE` refuses such a plan,
    /// which is exactly the `None` (bridge call) these sites had before.
    pub fn has_fp(&self) -> bool {
        self.segments.iter().any(|s| {
            matches!(
                s,
                ConcatSegment::Arg {
                    kind: ConcatArgKind::Float | ConcatArgKind::Double,
                    ..
                }
            )
        })
    }

    /// CONCAT21-1: the UTF16 twin a compiler may promote this plan's run-time
    /// UTF16 operands to, or `None` (see [`Self::wide_twin`]).
    pub fn promotion_twin(&self) -> Option<&JitConcatInlinePlan> {
        if self.wide {
            return None;
        }
        self.wide_twin.as_deref()
    }
}

/// `CRATONVM_JIT_CONCAT_INLINE` (JIT round 19 proposal wave 4, lane strgap4,
/// STRGAP19-3; default ON since the round-19 proposal round, `=0` turns it off; it needs `CRATONVM_JIT_CONCAT_LATIN1_FAST` (also default ON) for the bridge to refill the TLAB the inline path bumps): the single-pass backend lowers an admitted
/// concat site inline, with the bridge call as its decline edge. Read once
/// per indy site of a compile, no process global.
pub fn concat_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CONCAT_INLINE")
}

/// `CRATONVM_JIT_CONCAT_UTF16` (JIT round 19 proposal wave 7, lane str7,
/// STRGAP4-4; opt-in, default OFF): both tiers' inline concat lowers a
/// [`JitConcatInlinePlan::wide`] plan (a site whose literal text forces a
/// UTF16 result, e.g. a Greek capital omega before an `int`) instead of keeping its bridge call. Read
/// once per indy site of a compile, no process global.
pub fn concat_utf16_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_CONCAT_UTF16")
}

/// `CRATONVM_JIT_CONCAT_CODER_PROMOTE` (JIT round 21 wave 4, lane concat,
/// CONCAT21-1; opt-in, default OFF): both tiers' inline concat of a LATIN1
/// plan sends a run-time UTF16 operand (a UTF16 `String`, a `char` above
/// `0xFF`) to the plan's [`JitConcatInlinePlan::wide_twin`] passes -- a
/// second, UTF16 build of the same site, inline -- instead of declining the
/// whole evaluation to the bridge's general renderer (the `concat-wide-char`
/// / `concat-wide-str` decline rows). Independent of
/// `CRATONVM_JIT_CONCAT_UTF16` (which admits WIDE plans, whose literal text
/// is UTF16). Read once per indy site of a compile, no process global.
pub fn concat_coder_promote_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_CONCAT_CODER_PROMOTE")
}

/// `CRATONVM_JIT_CONCAT_FP_INLINE` (JIT round 21 wave 5, lane concat,
/// CONCAT21-3; opt-in, default OFF): both tiers' inline concat admits a plan
/// with `float` / `double` operands, rendering each through a leaf call to
/// [`jit_concat_render_fp`] (the VM's shared `Float`/`Double.toString`
/// formatter, the one the bridge already uses) into an RSP-relative scratch
/// area before the length pass. Off, such a plan is refused at compile time:
/// the bridge call these sites always had. Read once per site of a compile.
pub fn concat_fp_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_CONCAT_FP_INLINE")
}

/// Longest `Float` / `Double.toString` text the inline concat's scratch slot
/// holds (`-2.2250738585072014E-308` is 24 bytes; a longer answer declines).
pub const CONCAT_FP_TEXT_MAX: usize = 32;

/// CONCAT21-3: the leaf compiled code calls per `float` / `double` operand of
/// an inline concat: the text of `bits` (`is_double != 0`: the 64 bits of a
/// `double`, else the low 32 bits of a `float`) as ASCII at `out` (room for
/// [`CONCAT_FP_TEXT_MAX`] bytes), answering its length, or `u64::MAX` (the
/// caller declines to the bridge) for a text that does not fit or a panic.
/// Allocates only Rust memory, never the Java heap; no safepoint, no Java
/// code, no VM state.
///
/// The formatter is `cratonvm_types::java_double_to_string` /
/// `java_float_to_string`, which every VM concat path (interpreter, bridge,
/// Latin-1 fast path) already uses, so the inline answer is the bridge's by
/// construction. Its agreement with JDK 25's `DoubleToDecimal` is checked by
/// `R21ConcatRows` (`fact fp-vs-jdk`: concat vs `Double.toString` over a
/// random and an edge corpus).
///
/// # Safety
/// `out` must be valid for [`CONCAT_FP_TEXT_MAX`] bytes of writes.
pub unsafe extern "C" fn jit_concat_render_fp(bits: u64, is_double: u64, out: *mut u8) -> u64 {
    let text = std::panic::catch_unwind(|| {
        if is_double != 0 {
            cratonvm_types::java_double_to_string(f64::from_bits(bits))
        } else {
            // Cast: the float's bits are the slot's low 32.
            cratonvm_types::java_float_to_string(f32::from_bits(bits as u32))
        }
    });
    let Ok(text) = text else {
        return u64::MAX;
    };
    let b = text.as_bytes();
    if b.len() > CONCAT_FP_TEXT_MAX || !b.is_ascii() || out.is_null() {
        return u64::MAX;
    }
    // SAFETY: the caller's contract: `out` has room for CONCAT_FP_TEXT_MAX
    // bytes, and `b.len()` is at most that.
    unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), out, b.len()) };
    // Cast: at most CONCAT_FP_TEXT_MAX.
    b.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn an_int_and_string_recipe_is_split_in_order() {
        let recipe = units("v=\u{1}/\u{1}");
        let plan = JitConcatInlinePlan::from_recipe(
            &recipe,
            &[],
            "(ILjava/lang/String;)Ljava/lang/String;",
        )
        .expect("admitted");
        assert_eq!(
            plan.segments,
            vec![
                ConcatSegment::Literal(b"v=".to_vec()),
                ConcatSegment::Arg {
                    index: 0,
                    kind: ConcatArgKind::Int
                },
                ConcatSegment::Literal(b"/".to_vec()),
                ConcatSegment::Arg {
                    index: 1,
                    kind: ConcatArgKind::String
                },
            ]
        );
        assert_eq!(plan.literal_len, 3);
        assert_eq!(plan.arg_count, 2);
    }

    #[test]
    fn constants_merge_into_the_neighbouring_literal() {
        let recipe = units("<\u{2}|\u{1}\u{2}>");
        let k = units("K");
        let q = units("\u{e9}");
        let plan = JitConcatInlinePlan::from_recipe(
            &recipe,
            &[k.as_slice(), q.as_slice()],
            "(J)Ljava/lang/String;",
        )
        .expect("admitted");
        assert_eq!(
            plan.segments,
            vec![
                ConcatSegment::Literal(b"<K|".to_vec()),
                ConcatSegment::Arg {
                    index: 0,
                    kind: ConcatArgKind::Long
                },
                ConcatSegment::Literal(vec![0xE9, b'>']),
            ]
        );
        assert_eq!(plan.literal_len, 5);
    }

    #[test]
    fn every_admitted_kind_maps_from_its_descriptor() {
        let recipe = units("\u{1}\u{1}\u{1}\u{1}\u{1}\u{1}\u{1}");
        let plan = JitConcatInlinePlan::from_recipe(
            &recipe,
            &[],
            "(ZBCSIJLjava/lang/String;)Ljava/lang/String;",
        )
        .expect("admitted");
        let kinds: Vec<ConcatArgKind> = plan
            .segments
            .iter()
            .filter_map(|s| match s {
                ConcatSegment::Arg { kind, .. } => Some(*kind),
                ConcatSegment::Literal(_) | ConcatSegment::WideLiteral(_) => None,
            })
            .collect();
        use ConcatArgKind::*;
        assert_eq!(kinds, vec![Boolean, Int, Char, Int, Int, Long, String]);
        assert_eq!(plan.literal_len, 0);
    }

    #[test]
    fn what_needs_tostring_or_utf16_keeps_the_bridge() {
        let one = units("\u{1}");
        for d in [
            "(Ljava/lang/Object;)Ljava/lang/String;",
            "([I)Ljava/lang/String;",
            "(Ljava/lang/StringBuilder;)Ljava/lang/String;",
        ] {
            assert_eq!(JitConcatInlinePlan::from_recipe(&one, &[], d), None, "{d}");
        }
        // A literal above 0xFF: the result is always UTF16, so the plan is
        // WIDE (STRGAP4-4), which a compiler without CRATONVM_JIT_CONCAT_UTF16
        // refuses -- the bridge call this site always had.
        let p =
            JitConcatInlinePlan::from_recipe(&units("\u{3a9}\u{1}"), &[], "(I)Ljava/lang/String;")
                .expect("wide plan");
        assert!(p.wide);
        let wide = units("\u{3a9}");
        let p = JitConcatInlinePlan::from_recipe(
            &units("\u{2}"),
            &[wide.as_slice()],
            "()Ljava/lang/String;",
        )
        .expect("wide plan");
        assert!(p.wide);
    }

    /// STRGAP4-4: a wide plan keeps every literal as UTF-16 units (Latin-1
    /// text included, merged with the constants), in recipe order, and counts
    /// units; a Latin-1 recipe is never wide.
    #[test]
    fn a_wide_literal_makes_a_wide_plan_of_units() {
        let recipe = units("\u{3a9}=\u{1}<\u{2}>\u{1}");
        let k = units("k\u{e9}");
        let plan = JitConcatInlinePlan::from_recipe(
            &recipe,
            &[k.as_slice()],
            "(CLjava/lang/String;)Ljava/lang/String;",
        )
        .expect("admitted");
        assert!(plan.wide);
        assert_eq!(
            plan.segments,
            vec![
                ConcatSegment::WideLiteral(units("\u{3a9}=")),
                ConcatSegment::Arg {
                    index: 0,
                    kind: ConcatArgKind::Char
                },
                ConcatSegment::WideLiteral(units("<k\u{e9}>")),
                ConcatSegment::Arg {
                    index: 1,
                    kind: ConcatArgKind::String
                },
            ]
        );
        assert_eq!(plan.literal_len, 6);
        let narrow =
            JitConcatInlinePlan::from_recipe(&units("\u{e9}\u{1}"), &[], "(I)Ljava/lang/String;")
                .expect("admitted");
        assert!(!narrow.wide);
        assert_eq!(narrow.segments[0], ConcatSegment::Literal(vec![0xE9]));
    }

    #[test]
    fn mismatched_tags_and_oversize_sites_keep_the_bridge() {
        // Two argument tags, one operand.
        assert_eq!(
            JitConcatInlinePlan::from_recipe(&units("\u{1}\u{1}"), &[], "(I)Ljava/lang/String;"),
            None
        );
        // An unused operand.
        assert_eq!(
            JitConcatInlinePlan::from_recipe(&units("\u{1}"), &[], "(II)Ljava/lang/String;"),
            None
        );
        // A constant tag with no constant, and a constant with no tag.
        assert_eq!(
            JitConcatInlinePlan::from_recipe(&units("\u{2}"), &[], "()Ljava/lang/String;"),
            None
        );
        let k = units("k");
        assert_eq!(
            JitConcatInlinePlan::from_recipe(&units("x"), &[k.as_slice()], "()Ljava/lang/String;"),
            None
        );
        // Past the literal bound.
        let long_text = "a".repeat(CONCAT_INLINE_MAX_LITERAL + 1);
        assert_eq!(
            JitConcatInlinePlan::from_recipe(&units(&long_text), &[], "()Ljava/lang/String;"),
            None
        );
        // Past the operand bound.
        let n = CONCAT_INLINE_MAX_ARGS + 1;
        let recipe: Vec<u16> = vec![CONCAT_TAG_ARG; n];
        let desc = format!("({})Ljava/lang/String;", "I".repeat(n));
        assert_eq!(JitConcatInlinePlan::from_recipe(&recipe, &[], &desc), None);
        // A malformed descriptor.
        assert_eq!(
            JitConcatInlinePlan::from_recipe(&units("\u{1}"), &[], "(Q)V"),
            None
        );
    }

    /// CONCAT21-1: a LATIN1 plan with a `String` or `char` operand carries a
    /// wide twin of the same segments (literals widened, recipe order kept);
    /// an all-primitive plan and a wide plan carry none.
    #[test]
    fn r21_a_latin1_plan_with_a_string_or_char_has_a_wide_twin() {
        let plan = JitConcatInlinePlan::from_recipe(
            &units("v=\u{1}/\u{1}\u{e9}"),
            &[],
            "(ILjava/lang/String;)Ljava/lang/String;",
        )
        .expect("admitted");
        let twin = plan.promotion_twin().expect("a String operand can be UTF16");
        assert!(twin.wide);
        assert!(twin.wide_twin.is_none());
        assert_eq!(twin.literal_len, plan.literal_len);
        assert_eq!(twin.arg_count, plan.arg_count);
        assert_eq!(
            twin.segments,
            vec![
                ConcatSegment::WideLiteral(units("v=")),
                ConcatSegment::Arg {
                    index: 0,
                    kind: ConcatArgKind::Int
                },
                ConcatSegment::WideLiteral(units("/")),
                ConcatSegment::Arg {
                    index: 1,
                    kind: ConcatArgKind::String
                },
                ConcatSegment::WideLiteral(units("\u{e9}")),
            ]
        );
        let c = JitConcatInlinePlan::from_recipe(&units("\u{1}!"), &[], "(C)Ljava/lang/String;")
            .expect("admitted");
        assert!(c.promotion_twin().is_some(), "a char can be above 0xFF");
        let prims =
            JitConcatInlinePlan::from_recipe(&units("\u{1}:\u{1}"), &[], "(JZ)Ljava/lang/String;")
                .expect("admitted");
        assert!(prims.promotion_twin().is_none(), "never UTF16");
        let wide =
            JitConcatInlinePlan::from_recipe(&units("\u{3a9}\u{1}"), &[], "(C)Ljava/lang/String;")
                .expect("wide plan");
        assert!(wide.promotion_twin().is_none(), "already UTF16");
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_CODER_PROMOTE", None)],
            || assert!(!concat_coder_promote_enabled()),
        );
    }

    /// CONCAT21-3: `F` / `D` operands make an FP plan (refused by a compiler
    /// without `CRATONVM_JIT_CONCAT_FP_INLINE`), and the leaf renders the
    /// shared formatter's text, declining what does not fit its slot.
    #[test]
    fn r21_fp_operands_make_an_fp_plan_rendered_by_the_shared_formatter() {
        let plan =
            JitConcatInlinePlan::from_recipe(&units("f=\u{1} d=\u{1}"), &[], "(FD)Ljava/lang/String;")
                .expect("admitted");
        assert!(plan.has_fp());
        let kinds: Vec<ConcatArgKind> = plan
            .segments
            .iter()
            .filter_map(|s| match s {
                ConcatSegment::Arg { kind, .. } => Some(*kind),
                ConcatSegment::Literal(_) | ConcatSegment::WideLiteral(_) => None,
            })
            .collect();
        assert_eq!(kinds, vec![ConcatArgKind::Float, ConcatArgKind::Double]);
        let no_fp = JitConcatInlinePlan::from_recipe(&units("\u{1}"), &[], "(I)Ljava/lang/String;")
            .expect("admitted");
        assert!(!no_fp.has_fp());
        let render = |bits: u64, is_double: u64| -> Option<String> {
            let mut out = [0xAAu8; CONCAT_FP_TEXT_MAX + 8];
            // SAFETY: `out` has room for CONCAT_FP_TEXT_MAX bytes.
            let n = unsafe { jit_concat_render_fp(bits, is_double, out.as_mut_ptr()) };
            if n == u64::MAX {
                return None;
            }
            // Cast: at most CONCAT_FP_TEXT_MAX.
            let n = n as usize;
            assert!(out[n..].iter().all(|&b| b == 0xAA), "wrote past its text");
            Some(String::from_utf8(out[..n].to_vec()).expect("ASCII"))
        };
        for v in [
            0.0f64,
            -0.0,
            1.0,
            1e7,
            1.0e-3,
            9.999e-4,
            0.1 + 0.2,
            f64::MAX,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            5e-324,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let want = cratonvm_types::java_double_to_string(v);
            assert_eq!(render(v.to_bits(), 1).as_deref(), Some(want.as_str()), "{v}");
        }
        for v in [0.0f32, -1.5, 1e10, f32::MIN_POSITIVE, 1.4e-45, f32::NAN, f32::MAX] {
            let want = cratonvm_types::java_float_to_string(v);
            // Garbage in the high half: a float is the slot's low 32 bits.
            let bits = 0xDEAD_BEEF_0000_0000u64 | u64::from(v.to_bits());
            assert_eq!(render(bits, 0).as_deref(), Some(want.as_str()), "{v}");
        }
        assert_eq!(
            render(f64::MIN_POSITIVE.to_bits(), 1).as_deref(),
            Some("2.2250738585072014E-308")
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_FP_INLINE", None)],
            || assert!(!concat_fp_inline_enabled()),
        );
    }

    #[test]
    fn the_concat_inline_switch_follows_its_overrides() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_INLINE", Some("0"))],
            || assert!(!concat_inline_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_INLINE", Some("1"))],
            || assert!(concat_inline_enabled()),
        );
    }
}
