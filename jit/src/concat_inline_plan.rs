// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
    /// `Ljava/lang/Integer;` (CONCAT21-6): unboxed `value` or `"null"`.
    BoxedInt,
    /// `Ljava/lang/Long;` (CONCAT21-6): unboxed `value` or `"null"`.
    BoxedLong,
    /// `Ljava/lang/Character;` (CONCAT21-6): unboxed `value` or `"null"`.
    BoxedChar,
    /// `Ljava/lang/Boolean;` (CONCAT21-6): unboxed `value` or `"null"`.
    BoxedBool,
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
    /// `CRATONVM_JIT_CONCAT_UTF16`, opt-in, default OFF): the literal text holds a
    /// unit above `0xFF`, so EVERY result of the site is UTF16 (HotSpot's
    /// `StringConcatHelper` ORs the literal's coder into the result's, and a
    /// non-Latin-1 char can never be compressed away). Its literals are
    /// [`ConcatSegment::WideLiteral`]s, a `char` operand of any value renders
    /// inline, and a UTF16 `String` operand is copied rather than declined. A
    /// compiler without the switch refuses a wide plan, which is exactly the
    /// `None` this site answered before.
    pub wide: bool,
    /// JIT round 21 wave 4, lane concat (CONCAT21-1; the compilers read
    /// `CRATONVM_JIT_CONCAT_CODER_PROMOTE`, default ON since r23): the UTF16 twin of a
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
        "Ljava/lang/Integer;" => Some(ConcatArgKind::BoxedInt),
        "Ljava/lang/Long;" => Some(ConcatArgKind::BoxedLong),
        "Ljava/lang/Character;" => Some(ConcatArgKind::BoxedChar),
        "Ljava/lang/Boolean;" => Some(ConcatArgKind::BoxedBool),
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
            && kinds.iter().any(|k| {
                matches!(
                    k,
                    ConcatArgKind::String | ConcatArgKind::Char | ConcatArgKind::BoxedChar
                )
            });
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

    /// CONCAT21-6: does the plan unbox a descriptor-typed primitive wrapper?
    pub fn has_boxed(&self) -> bool {
        self.segments.iter().any(|s| {
            matches!(
                s,
                ConcatSegment::Arg {
                    kind: ConcatArgKind::BoxedInt
                        | ConcatArgKind::BoxedLong
                        | ConcatArgKind::BoxedChar
                        | ConcatArgKind::BoxedBool,
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
    true
}

/// `CRATONVM_JIT_CONCAT_UTF16` (JIT round 19 proposal wave 7, lane str7,
/// STRGAP4-4; default on, default on): both tiers' inline concat lowers a
/// [`JitConcatInlinePlan::wide`] plan (a site whose literal text forces a
/// UTF16 result, e.g. a Greek capital omega before an `int`) instead of keeping its bridge call. Read
/// once per indy site of a compile, no process global.
pub fn concat_utf16_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_CONCAT_CODER_PROMOTE` (JIT round 21 wave 4, lane concat,
/// CONCAT21-1; default ON since the r23 A/B, `=0` off): both tiers' inline concat of a LATIN1
/// plan sends a run-time UTF16 operand (a UTF16 `String`, a `char` above
/// `0xFF`) to the plan's [`JitConcatInlinePlan::wide_twin`] passes -- a
/// second, UTF16 build of the same site, inline -- instead of declining the
/// whole evaluation to the bridge's general renderer (the `concat-wide-char`
/// / `concat-wide-str` decline rows). Independent of
/// `CRATONVM_JIT_CONCAT_UTF16` (which admits WIDE plans, whose literal text
/// is UTF16). Read once per indy site of a compile, no process global.
pub fn concat_coder_promote_enabled() -> bool {
    // R23-AB-FLIP: default ON since the JIT r23 triage A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_CONCAT_FP_INLINE` (JIT round 21 wave 5, lane concat,
/// CONCAT21-3; default ON (flipped; `=0` kills)): both tiers' inline concat admits a plan
/// with `float` / `double` operands, rendering each through a leaf call to
/// [`jit_concat_render_fp`] (the VM's shared `Float`/`Double.toString`
/// formatter, the one the bridge already uses) into an RSP-relative scratch
/// area before the length pass. Off, such a plan is refused at compile time:
/// the bridge call these sites always had. Read once per site of a compile.
pub fn concat_fp_inline_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
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
mod tests;
