// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Integer value-range analysis over machine-integer semantics.
//!
//! Two things live here, and the split is deliberate:
//!
//! 1. **[`Range`] — the lattice.** An interval over a *machine* integer of a
//!    declared [`IntWidth`] (32-bit for JVM `int`, 64-bit for `long`), with
//!    every arithmetic transfer function written so that **wrap is an
//!    explicit case**. This is the part that makes the analysis usable for
//!    bounds-check elimination: an analysis that quietly reasons in
//!    mathematical integers concludes `i + 1 > i`, which is false at
//!    `Integer.MAX_VALUE`, and that single wrong inequality is enough to
//!    delete a real bounds check and turn an array store into an
//!    out-of-bounds heap write.
//!
//! 2. **[`analyze_graph`] — the sea-of-nodes evaluator.** A bounded,
//!    widening fixpoint that assigns a [`Range`] to every `Int`/`Long`-typed
//!    node of a [`crate::ir::Graph`].
//!
//! ## Relationship to `scev::IntRange`
//!
//! `jit/src/scev.rs` already carries an interval lattice ([`IntRange`]) and a
//! *counted-loop* proof engine on top of it, and `x64/bce.rs` already consumes
//! both (see `docs/jit/range-analysis.md`). That lattice is `int`-only and
//! exists to answer one question — "what values can this loop's induction
//! variable take?".
//!
//! This module is its wider sibling:
//!
//! | | `scev::IntRange` | `range_analysis::Range` |
//! |---|---|---|
//! | domain | JVM `int` only | `int` **and** `long`, width carried per value |
//! | arithmetic | `add`/`sub`/`scale`/`neg` | + `mul`/`div`/`rem`/`and`/`or`/`xor`/`shl`/`shr`/`ushr` + the JVMS narrowing conversions |
//! | fixpoint | none (closed-form per loop) | bounded worklist with widening |
//! | consumer | counted-loop bounds proofs | SSA node ranges; the guard-dominated BCE reason in `x64/bce.rs` |
//!
//! They agree on the important thing — top proves nothing, and a wrapping
//! computation is never silently treated as exact — and [`Range::to_scev`] /
//! [`Range::from_scev`] convert between them so a proof can hand facts across
//! without a second, divergent notion of "unknown".
//!
//! ## The overflow rule, stated once
//!
//! Every binary transfer function computes the *exact* result interval in
//! `i128` (wide enough that no intermediate can itself overflow: the extreme
//! product of two `i64` endpoints is `2^126`), and then narrows:
//!
//! * result interval fits in the declared width → that interval, exactly;
//! * result interval does **not** fit → [`Range::top`], because the machine
//!   operation wraps and the wrapped value can be anything in the width.
//!
//! Top is sound (a wrapped `int` is still an `int`) and useless for a proof,
//! which is exactly the intent. The `*_no_wrap` variants expose the same
//! computation but answer `None` instead of top, for callers that must
//! *refuse* rather than degrade.

use crate::ir::{node_id_opt, CmpOp, Graph, IrType, MemKind, NodeId, Op};
use crate::ir_schedule::Schedule;
use crate::scev::IntRange;

// ===========================================================================
// Width
// ===========================================================================

/// The machine width a [`Range`] is interpreted at.
///
/// A range is only meaningful together with its width: `[0, 2^40]` is a
/// perfectly ordinary `long` range and an impossible `int` one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntWidth {
    /// JVM `int` (and `boolean`/`byte`/`char`/`short`, which all compute as
    /// `int`): 32-bit wrapping two's complement.
    W32,
    /// JVM `long`: 64-bit wrapping two's complement.
    W64,
}

impl IntWidth {
    /// Number of value bits.
    pub const fn bits(self) -> u32 {
        match self {
            IntWidth::W32 => 32,
            IntWidth::W64 => 64,
        }
    }

    /// Smallest representable value.
    pub const fn min(self) -> i64 {
        match self {
            IntWidth::W32 => i32::MIN as i64,
            IntWidth::W64 => i64::MIN,
        }
    }

    /// Largest representable value.
    pub const fn max(self) -> i64 {
        match self {
            IntWidth::W32 => i32::MAX as i64,
            IntWidth::W64 => i64::MAX,
        }
    }

    /// The mask JVMS 6.5 applies to a shift count (`ishl` uses the low 5 bits,
    /// `lshl` the low 6). Getting this wrong is not a precision bug — it would
    /// make `x << 32` read as "shift by 32" where the machine shifts by 0.
    pub const fn shift_mask(self) -> u32 {
        self.bits() - 1
    }
}

// ===========================================================================
// The lattice
// ===========================================================================

/// An element of the value-range lattice: a closed interval over a machine
/// integer of a known [`IntWidth`], or bottom.
///
/// * bottom — `iv == None` — "no value at all": unreachable code, or two facts
///   that contradict each other. [`Range::is_empty`].
/// * `[lo, hi]` with `lo <= hi` — every value the expression can take is in
///   here.
/// * top — `[width.min(), width.max()]` — proves nothing. [`Range::is_top`].
///
/// There is deliberately no "probably" element. Anything unproven is top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    width: IntWidth,
    /// `None` is bottom. `Some((lo, hi))` maintains `lo <= hi` and both
    /// endpoints inside `width`.
    iv: Option<(i64, i64)>,
}

impl Range {
    // ---- constructors ----------------------------------------------------

    /// Top for `width`: every representable value.
    pub fn top(width: IntWidth) -> Range {
        Range {
            width,
            iv: Some((width.min(), width.max())),
        }
    }

    /// Bottom for `width`.
    pub fn empty(width: IntWidth) -> Range {
        Range { width, iv: None }
    }

    /// The interval `[lo, hi]`, clamped into `width` and normalised to bottom
    /// when `lo > hi`.
    ///
    /// Clamping (rather than rejecting) is safe because the caller's `lo`/`hi`
    /// are always claims about a value that is *already* representable at
    /// `width`; the clamp only discards the unrepresentable part of the claim.
    pub fn exact(width: IntWidth, lo: i64, hi: i64) -> Range {
        let lo = lo.max(width.min());
        let hi = hi.min(width.max());
        if lo > hi {
            Range { width, iv: None }
        } else {
            Range {
                width,
                iv: Some((lo, hi)),
            }
        }
    }

    /// The singleton `{v}` at `width`.
    pub fn constant(width: IntWidth, v: i64) -> Range {
        Range::exact(width, v, v)
    }

    /// The range of a JVM array length: `[0, Integer.MAX_VALUE]`.
    ///
    /// Non-negative by JVMS (`newarray` throws `NegativeArraySizeException`
    /// below zero), and bounded above because the length lives in an `int`.
    pub fn array_length() -> Range {
        Range::exact(IntWidth::W32, 0, i32::MAX as i64)
    }

    /// The `int` range of a value loaded through `kind`, exploiting the JVMS
    /// sub-word element types. `Int`/`Long`/`Float`/`Double`/`Ref` narrow
    /// nothing and answer `None`.
    pub fn of_mem_kind(kind: MemKind) -> Option<Range> {
        match kind {
            MemKind::Byte => Some(Range::exact(IntWidth::W32, -128, 127)),
            MemKind::Char => Some(Range::exact(IntWidth::W32, 0, 65535)),
            MemKind::Short => Some(Range::exact(IntWidth::W32, -32768, 32767)),
            _ => None,
        }
    }

    // ---- accessors -------------------------------------------------------

    /// The width this range is interpreted at.
    pub fn width(self) -> IntWidth {
        self.width
    }

    /// Whether this is bottom.
    pub fn is_empty(self) -> bool {
        self.iv.is_none()
    }

    /// Whether this is top — i.e. proves nothing.
    pub fn is_top(self) -> bool {
        self.iv == Some((self.width.min(), self.width.max()))
    }

    /// Inclusive lower endpoint, or `None` at bottom.
    pub fn lo(self) -> Option<i64> {
        self.iv.map(|(lo, _)| lo)
    }

    /// Inclusive upper endpoint, or `None` at bottom.
    pub fn hi(self) -> Option<i64> {
        self.iv.map(|(_, hi)| hi)
    }

    /// The single value admitted, when exactly one is.
    pub fn as_constant(self) -> Option<i64> {
        match self.iv {
            Some((lo, hi)) if lo == hi => Some(lo),
            _ => None,
        }
    }

    /// Whether this range admits at least one value and every value it admits
    /// is `>= 0`.
    ///
    /// **The only non-negativity predicate on [`Range`], and deliberately so.**
    /// There used to be a vacuous twin, `is_non_negative`, answering `true` at
    /// bottom on the grounds that bottom means the value does not exist and
    /// every claim about it therefore holds. Correct as logic and a trap as an
    /// API: a consumer reads "every value is non-negative" as permission to
    /// delete a bounds check, and gets that permission for a value the analysis
    /// believes cannot occur — a "cannot occur" which, if it is ever wrong, is
    /// an unchecked index. Both of its callers wrote
    /// `!r.is_empty() && r.is_non_negative()` by hand, which is this function,
    /// so the vacuous form has been REMOVED rather than documented and the next
    /// caller cannot reach for it by accident.
    ///
    /// A caller that genuinely wants the vacuous reading is asking about the
    /// LATTICE rather than about a value, and writes [`Range::is_empty`]
    /// itself, where the intent is visible at the call site.
    pub fn definitely_non_negative(self) -> bool {
        match self.iv {
            None => false,
            Some((lo, _)) => lo >= 0,
        }
    }

    /// Whether `other`'s values are all admitted by `self` (i.e. `other` is
    /// lower in the lattice). Width mismatch answers `false`.
    pub fn contains_all(self, other: Range) -> bool {
        if self.width != other.width {
            return false;
        }
        match (self.iv, other.iv) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some((al, ah)), Some((bl, bh))) => al <= bl && bh <= ah,
        }
    }

    // ---- lattice ---------------------------------------------------------

    /// Join — the interval hull. The control-flow merge operator.
    ///
    /// A width mismatch cannot be hulled meaningfully, so it degrades to
    /// `top` of `self`'s width. That is the safe direction: top proves
    /// nothing.
    pub fn join(self, other: Range) -> Range {
        if self.width != other.width {
            return Range::top(self.width);
        }
        match (self.iv, other.iv) {
            (None, _) => other,
            (_, None) => self,
            (Some((al, ah)), Some((bl, bh))) => Range::exact(self.width, al.min(bl), ah.max(bh)),
        }
    }

    /// Meet — intersection. Two independent facts about the same value combine
    /// here, and a contradiction becomes bottom rather than a choice between
    /// them.
    pub fn meet(self, other: Range) -> Range {
        if self.width != other.width {
            return self;
        }
        match (self.iv, other.iv) {
            (None, _) | (_, None) => Range::empty(self.width),
            (Some((al, ah)), Some((bl, bh))) => Range::exact(self.width, al.max(bl), ah.min(bh)),
        }
    }

    /// Widening: the operator that makes an ascending fixpoint terminate.
    ///
    /// `self` is the previous value, `next` the newly computed one. Any
    /// endpoint that moved *outwards* is thrown to the extreme of the width,
    /// so a chain of merges can only ever take a value up the lattice a
    /// bounded number of times (at most twice per endpoint) before it
    /// saturates at top.
    ///
    /// Without this, an interval lattice over 32-bit integers has height
    /// `2^32` and an unbounded walk hangs the compiler thread — which this
    /// VM's watchdog reports as a VM hang, not as a compiler bug.
    pub fn widen(self, next: Range) -> Range {
        if self.width != next.width {
            return Range::top(self.width);
        }
        let (al, ah) = match self.iv {
            None => return next,
            Some(v) => v,
        };
        let (bl, bh) = match next.iv {
            None => return self,
            Some(v) => v,
        };
        let lo = if bl < al {
            self.width.min()
        } else {
            al.min(bl)
        };
        let hi = if bh > ah {
            self.width.max()
        } else {
            ah.max(bh)
        };
        Range::exact(self.width, lo, hi)
    }

    // ---- narrowing by a comparison (the edge-narrowing primitives) -------
    //
    // Each answers: "given that `self` COMPARES <op> against some value drawn
    // from `other`, what is left of `self`?" They are the half of B3 that has
    // to be right -- a narrowing that keeps one value too many is merely a
    // missed optimisation, but one that drops a value that can actually occur
    // deletes a live bounds check, which is the out-of-bounds heap write this
    // module's header opens by naming.
    //
    // Two rules run through all six:
    //
    // * The bound comes from the *opposite* endpoint of `other`. `self < other`
    //   admits every `self` below the LARGEST value `other` can take, so the
    //   new upper bound is `other.hi - 1` and not `other.lo - 1`. Taking the
    //   near endpoint would be the unsound direction.
    // * The plus/minus one is CHECKED. `x < other` where `other.hi == i64::MIN`
    //   (a `long` at the bottom of its width) has no solution, and computing
    //   `i64::MIN - 1` to say so would itself overflow -- in release builds,
    //   silently, to `i64::MAX`, inverting the bound. `checked_sub` answering
    //   `None` IS the "no value satisfies this" case, so it maps to bottom.
    //
    // A width mismatch returns `self` unchanged: two ranges of different widths
    // are not comparable and refusing to narrow proves less, which is the safe
    // direction.

    /// `self` restricted to values `<` some value of `other`.
    pub fn narrow_lt(self, other: Range) -> Range {
        if self.width != other.width {
            return self;
        }
        let Some((_, bh)) = other.iv else {
            // `other` is bottom: the comparison is against a value that cannot
            // occur, so this edge is unreachable and nothing survives.
            return Range::empty(self.width);
        };
        match bh.checked_sub(1) {
            Some(hi) => self.meet(Range::exact(self.width, self.width.min(), hi)),
            None => Range::empty(self.width),
        }
    }

    /// `self` restricted to values `<=` some value of `other`.
    pub fn narrow_le(self, other: Range) -> Range {
        if self.width != other.width {
            return self;
        }
        let Some((_, bh)) = other.iv else {
            return Range::empty(self.width);
        };
        self.meet(Range::exact(self.width, self.width.min(), bh))
    }

    /// `self` restricted to values `>` some value of `other`.
    pub fn narrow_gt(self, other: Range) -> Range {
        if self.width != other.width {
            return self;
        }
        let Some((bl, _)) = other.iv else {
            return Range::empty(self.width);
        };
        match bl.checked_add(1) {
            Some(lo) => self.meet(Range::exact(self.width, lo, self.width.max())),
            None => Range::empty(self.width),
        }
    }

    /// `self` restricted to values `>=` some value of `other`.
    pub fn narrow_ge(self, other: Range) -> Range {
        if self.width != other.width {
            return self;
        }
        let Some((bl, _)) = other.iv else {
            return Range::empty(self.width);
        };
        self.meet(Range::exact(self.width, bl, self.width.max()))
    }

    /// `self` restricted to values equal to some value of `other` -- plain
    /// intersection.
    pub fn narrow_eq(self, other: Range) -> Range {
        if self.width != other.width {
            return self;
        }
        self.meet(other)
    }

    /// `self` restricted to values `!=` the value of `other`.
    ///
    /// An interval lattice cannot represent a hole, so this proves something
    /// only when `other` is a CONSTANT sitting on one of `self`'s endpoints --
    /// then the endpoint moves in by one. Every other shape returns `self`
    /// unchanged, which is the honest answer rather than a fabricated one.
    ///
    /// `x != 0` on a `[0, n]` is the shape that makes this worth having: it is
    /// what a zero guard leaves behind, and it turns `[0, n]` into `[1, n]`.
    pub fn narrow_ne(self, other: Range) -> Range {
        if self.width != other.width {
            return self;
        }
        let (Some(v), Some((lo, hi))) = (other.as_constant(), self.iv) else {
            return self;
        };
        if v == lo && v == hi {
            // The only value `self` could take is the one excluded.
            return Range::empty(self.width);
        }
        if v == lo {
            // `lo + 1` cannot overflow: `lo < hi <= width.max()` here.
            return Range::exact(self.width, lo + 1, hi);
        }
        if v == hi {
            return Range::exact(self.width, lo, hi - 1);
        }
        self
    }

    /// Dispatch [`Range::narrow_lt`] and friends by [`CmpOp`], for the
    /// `self <op> other` reading.
    pub fn narrow_by(self, op: CmpOp, other: Range) -> Range {
        match op {
            CmpOp::Lt => self.narrow_lt(other),
            CmpOp::Le => self.narrow_le(other),
            CmpOp::Gt => self.narrow_gt(other),
            CmpOp::Ge => self.narrow_ge(other),
            CmpOp::Eq => self.narrow_eq(other),
            CmpOp::Ne => self.narrow_ne(other),
        }
    }

    // ---- interop with `scev::IntRange` -----------------------------------

    /// This range as a `scev::IntRange`, for handing a fact to the
    /// counted-loop proof engine. `None` for a 64-bit range — `IntRange` is an
    /// `int` lattice and silently truncating into it would be exactly the
    /// class of bug this module exists to prevent.
    pub fn to_scev(self) -> Option<IntRange> {
        if self.width != IntWidth::W32 {
            return None;
        }
        Some(match self.iv {
            None => IntRange::empty(),
            // Cast: both endpoints are inside W32 by the `exact` invariant.
            Some((lo, hi)) => IntRange::new(lo as i32, hi as i32),
        })
    }

    /// A `scev::IntRange` lifted into this lattice at [`IntWidth::W32`].
    pub fn from_scev(r: IntRange) -> Range {
        match (r.lo(), r.hi()) {
            // Widening: i32 endpoints into the i64 interval representation.
            (Some(lo), Some(hi)) => Range::exact(IntWidth::W32, lo as i64, hi as i64),
            _ => Range::empty(IntWidth::W32),
        }
    }

    // ---- internals -------------------------------------------------------

    /// Endpoints widened to `i128`, or `None` at bottom.
    fn wide(self) -> Option<(i128, i128)> {
        // Widening: i64 endpoints into the i128 exact-arithmetic domain.
        self.iv.map(|(lo, hi)| (lo as i128, hi as i128))
    }

    /// Narrow an exact `i128` interval back to `width`.
    ///
    /// The single place the overflow decision is made: an interval that does
    /// not fit is the machine operation wrapping, and a wrapped value can be
    /// anything, so the answer is top.
    fn narrow(width: IntWidth, lo: i128, hi: i128) -> Range {
        match Range::narrow_exact(width, lo, hi) {
            Some(r) => r,
            None => Range::top(width),
        }
    }

    /// [`Range::narrow`] that refuses instead of degrading: `None` when the
    /// exact interval is not representable at `width`.
    fn narrow_exact(width: IntWidth, lo: i128, hi: i128) -> Option<Range> {
        if lo > hi {
            return Some(Range::empty(width));
        }
        // Widening: width endpoints into the i128 comparison domain.
        if lo < width.min() as i128 || hi > width.max() as i128 {
            return None;
        }
        Some(Range {
            width,
            // Cast: bounds-checked against `width` immediately above.
            iv: Some((lo as i64, hi as i64)),
        })
    }

    /// Binary-operation preamble: both operands' `i128` endpoints, or the
    /// short-circuit answer when either side is bottom or the widths disagree.
    fn binary_operands(self, other: Range) -> Result<((i128, i128), (i128, i128)), Range> {
        if self.width != other.width {
            return Err(Range::top(self.width));
        }
        match (self.wide(), other.wide()) {
            (Some(a), Some(b)) => Ok((a, b)),
            // Bottom in, bottom out: the operation never executes.
            _ => Err(Range::empty(self.width)),
        }
    }

    // ---- arithmetic ------------------------------------------------------

    /// `iadd` / `ladd`. Wraps to top.
    pub fn add(self, other: Range) -> Range {
        match self.binary_operands(other) {
            Err(short) => short,
            Ok(((al, ah), (bl, bh))) => Range::narrow(self.width, al + bl, ah + bh),
        }
    }

    /// Exact addition: `None` when some sum is not representable, i.e. when
    /// the machine addition could be observed wrapping.
    pub fn add_no_wrap(self, other: Range) -> Option<Range> {
        match self.binary_operands(other) {
            Err(short) if short.is_empty() => Some(short),
            Err(_) => None,
            Ok(((al, ah), (bl, bh))) => Range::narrow_exact(self.width, al + bl, ah + bh),
        }
    }

    /// `isub` / `lsub`. Wraps to top.
    pub fn sub(self, other: Range) -> Range {
        match self.binary_operands(other) {
            Err(short) => short,
            Ok(((al, ah), (bl, bh))) => Range::narrow(self.width, al - bh, ah - bl),
        }
    }

    /// Exact subtraction; `None` when a difference is not representable.
    pub fn sub_no_wrap(self, other: Range) -> Option<Range> {
        match self.binary_operands(other) {
            Err(short) if short.is_empty() => Some(short),
            Err(_) => None,
            Ok(((al, ah), (bl, bh))) => Range::narrow_exact(self.width, al - bh, ah - bl),
        }
    }

    /// Exact `self + k`; `None` when the shift is not representable.
    pub fn offset_no_wrap(self, k: i64) -> Option<Range> {
        self.add_no_wrap(Range::constant(self.width, k))
    }

    /// `imul` / `lmul`. Wraps to top.
    pub fn mul(self, other: Range) -> Range {
        match self.binary_operands(other) {
            Err(short) => short,
            Ok(((al, ah), (bl, bh))) => {
                let p = [al * bl, al * bh, ah * bl, ah * bh];
                let lo = p.iter().copied().fold(i128::MAX, |acc, x| acc.min(x));
                let hi = p.iter().copied().fold(i128::MIN, |acc, x| acc.max(x));
                Range::narrow(self.width, lo, hi)
            }
        }
    }

    /// `ineg` / `lneg`. Wraps to top (`-i32::MIN` is not an `int`).
    pub fn neg(self) -> Range {
        match self.wide() {
            None => self,
            Some((lo, hi)) => Range::narrow(self.width, -hi, -lo),
        }
    }

    /// Exact negation; `None` when the range contains the width's minimum.
    pub fn neg_no_wrap(self) -> Option<Range> {
        match self.wide() {
            None => Some(self),
            Some((lo, hi)) => Range::narrow_exact(self.width, -hi, -lo),
        }
    }

    /// `idiv` / `ldiv` (truncating toward zero, JVMS 6.5).
    ///
    /// Division by zero throws, so the zero divisor contributes no values; a
    /// divisor range of exactly `{0}` therefore yields bottom. The single
    /// overflow case (`MIN / -1`) is caught by the narrowing, not special-cased.
    pub fn div(self, other: Range) -> Range {
        let ((al, ah), (bl, bh)) = match self.binary_operands(other) {
            Err(short) => return short,
            Ok(v) => v,
        };
        if bl == 0 && bh == 0 {
            return Range::empty(self.width);
        }
        if let Some(c) = other.as_constant() {
            if c != 0 {
                // Widening: the divisor into the i128 exact-arithmetic domain.
                let c = c as i128;
                let (q1, q2) = (al / c, ah / c);
                return Range::narrow(self.width, q1.min(q2), q1.max(q2));
            }
        }
        // Unknown divisor: |a / b| <= |a| for every admissible b.
        let m = al.abs().max(ah.abs());
        Range::narrow(self.width, -m, m)
    }

    /// `irem` / `lrem`.
    ///
    /// JVMS: the result has the sign of the *dividend* and a magnitude strictly
    /// below `|divisor|`. Both halves are used — `i % n` with a non-negative
    /// `i` is the narrowing that makes a hashed array index provable.
    pub fn rem(self, other: Range) -> Range {
        let ((al, ah), (bl, bh)) = match self.binary_operands(other) {
            Err(short) => return short,
            Ok(v) => v,
        };
        if bl == 0 && bh == 0 {
            return Range::empty(self.width);
        }
        // Largest |divisor| the range admits; `bound` is the largest possible
        // |result|. `bl.abs()` is exact here because the endpoints are already
        // in i128 (`i64::MIN.abs()` would panic; `(i64::MIN as i128).abs()`
        // does not).
        let m = bl.abs().max(bh.abs());
        let bound = (m - 1).max(0);
        let mut lo = if al >= 0 { 0 } else { -bound };
        let mut hi = if ah <= 0 { 0 } else { bound };
        // |a % b| <= |a| as well.
        if al >= 0 {
            hi = hi.min(ah);
        }
        if ah <= 0 {
            lo = lo.max(al);
        }
        Range::narrow(self.width, lo, hi)
    }

    // ---- bitwise ---------------------------------------------------------

    /// `iand` / `land`.
    ///
    /// The load-bearing case is a non-negative operand: `x & m` with `m >= 0`
    /// can only clear bits of `m`, so the result is in `[0, m]` whatever `x`
    /// is. That is what makes `hash & (table.length - 1)` a provable index.
    pub fn and(self, other: Range) -> Range {
        let ((al, ah), (bl, bh)) = match self.binary_operands(other) {
            Err(short) => return short,
            Ok(v) => v,
        };
        match (al >= 0, bl >= 0) {
            (true, true) => Range::narrow(self.width, 0, ah.min(bh)),
            (true, false) => Range::narrow(self.width, 0, ah),
            (false, true) => Range::narrow(self.width, 0, bh),
            // Both operands may be negative. If both are *entirely* negative
            // the sign bit is set in each, so it is set in the result.
            (false, false) => {
                if ah < 0 && bh < 0 {
                    // Widening: width minimum into the i128 domain.
                    Range::narrow(self.width, self.width.min() as i128, -1)
                } else {
                    Range::top(self.width)
                }
            }
        }
    }

    /// `ior` / `lor`. Modelled only for two non-negative operands, where the
    /// result cannot exceed the all-ones fill of the wider endpoint.
    pub fn or(self, other: Range) -> Range {
        let ((al, ah), (bl, bh)) = match self.binary_operands(other) {
            Err(short) => return short,
            Ok(v) => v,
        };
        if al >= 0 && bl >= 0 {
            Range::narrow(self.width, al.max(bl), fill_ones(ah.max(bh)))
        } else {
            Range::top(self.width)
        }
    }

    /// `ixor` / `lxor`. Modelled only for two non-negative operands.
    pub fn xor(self, other: Range) -> Range {
        let ((al, ah), (bl, bh)) = match self.binary_operands(other) {
            Err(short) => return short,
            Ok(v) => v,
        };
        if al >= 0 && bl >= 0 {
            Range::narrow(self.width, 0, fill_ones(ah.max(bh)))
        } else {
            Range::top(self.width)
        }
    }

    /// `ishl` / `lshl`. The shift count is masked per JVMS 6.5 and must be a
    /// compile-time constant; anything else is top.
    pub fn shl(self, count: Range) -> Range {
        let (lo, hi) = match self.wide() {
            None => return self,
            Some(v) => v,
        };
        let Some(c) = count.as_constant() else {
            return Range::top(self.width);
        };
        // Cast: JVMS masks the shift count to the low 5 (int) / 6 (long) bits.
        let s = (c as u64 as u32) & self.width.shift_mask();
        if s == 0 {
            return self;
        }
        let factor = 1i128 << s;
        Range::narrow(self.width, lo * factor, hi * factor)
    }

    /// `ishr` / `lshr` — arithmetic (sign-propagating) right shift.
    ///
    /// Never wraps: an arithmetic right shift moves a value toward `0`
    /// (non-negative input) or toward `-1` (negative input), so even an
    /// unknown count keeps the result inside `[min(lo, 0), max(hi, -1)]`.
    pub fn shr(self, count: Range) -> Range {
        let (lo, hi) = match self.wide() {
            None => return self,
            Some(v) => v,
        };
        match count.as_constant() {
            Some(c) => {
                // Cast: JVMS shift-count masking, as in `shl`.
                let s = (c as u64 as u32) & self.width.shift_mask();
                Range::narrow(self.width, lo >> s, hi >> s)
            }
            None => Range::narrow(self.width, lo.min(0), hi.max(-1)),
        }
    }

    /// `iushr` / `lushr` — logical right shift, evaluated on the *unsigned*
    /// bit pattern of the declared width.
    ///
    /// An unknown count is top: the count could be `0`, in which case the
    /// result is the (possibly negative) input unchanged.
    pub fn ushr(self, count: Range) -> Range {
        let (lo, hi) = match self.wide() {
            None => return self,
            Some(v) => v,
        };
        let Some(c) = count.as_constant() else {
            return Range::top(self.width);
        };
        // Cast: JVMS shift-count masking, as in `shl`.
        let s = (c as u64 as u32) & self.width.shift_mask();
        if s == 0 {
            return self;
        }
        if lo >= 0 {
            // A non-negative value's logical and arithmetic shifts agree.
            Range::narrow(self.width, lo >> s, hi >> s)
        } else {
            // The sign bit is shifted into the value bits: the result is a
            // non-negative number below 2^(bits - s).
            Range::narrow(self.width, 0, (1i128 << (self.width.bits() - s)) - 1)
        }
    }

    // ---- conversions -----------------------------------------------------

    /// `i2l` — always exact.
    pub fn i2l(self) -> Range {
        match self.wide() {
            None => Range::empty(IntWidth::W64),
            Some((lo, hi)) => Range::narrow(IntWidth::W64, lo, hi),
        }
    }

    /// `l2i` — truncation. Exact when the source range already fits in 32
    /// bits, top otherwise.
    pub fn l2i(self) -> Range {
        match self.wide() {
            None => Range::empty(IntWidth::W32),
            Some((lo, hi)) => Range::narrow(IntWidth::W32, lo, hi),
        }
    }

    /// `i2b` / `i2c` / `i2s` — truncate to the sub-word type, then widen back
    /// to `int` with that type's sign rule. Exact when the source already fits.
    fn truncate_to(self, lo_bound: i64, hi_bound: i64) -> Range {
        let target = Range::exact(IntWidth::W32, lo_bound, hi_bound);
        match self.iv {
            None => Range::empty(IntWidth::W32),
            Some((lo, hi)) if lo >= lo_bound && hi <= hi_bound => {
                Range::exact(IntWidth::W32, lo, hi)
            }
            Some(_) => target,
        }
    }

    /// `i2b`.
    pub fn i2b(self) -> Range {
        self.truncate_to(-128, 127)
    }

    /// `i2c`.
    pub fn i2c(self) -> Range {
        self.truncate_to(0, 65535)
    }

    /// `i2s`.
    pub fn i2s(self) -> Range {
        self.truncate_to(-32768, 32767)
    }
}

/// The smallest `2^k - 1` that is `>= x`, for `x >= 0`.
///
/// Used as the upper bound of a bitwise `or`/`xor` of two non-negative
/// operands: if `a <= ah` then `a`'s highest set bit is no higher than `ah`'s,
/// so `a | b` cannot exceed the all-ones fill of the higher of the two.
fn fill_ones(x: i128) -> i128 {
    if x <= 0 {
        return 0;
    }
    let mut fill: i128 = 1;
    while fill - 1 < x {
        fill <<= 1;
    }
    fill - 1
}

// ===========================================================================
// Sea-of-nodes evaluation
// ===========================================================================

/// Rounds the node fixpoint may take before it gives up.
///
/// Cycles in the value graph can only run through `Op::Phi` (every other data
/// edge points at a strictly earlier definition), and [`WIDEN_AFTER_ROUND`]
/// widens every φ from that round on, so a converging graph converges well
/// inside this. The cap exists for the graph that does not: an unbounded
/// lattice walk pins the compiler thread, and this VM's watchdog reports that
/// as a **VM hang**, not as a compiler bug.
pub const MAX_GRAPH_ROUNDS: usize = 16;

/// The round from which φ nodes widen instead of merging exactly.
pub const WIDEN_AFTER_ROUND: usize = 4;

/// Per-node ranges for one [`Graph`].
#[derive(Clone, Debug)]
pub struct GraphRanges {
    ranges: Vec<Range>,
    converged: bool,
}

impl GraphRanges {
    /// The range of `id`, or top when the id is out of range.
    ///
    /// The width is the one implied by the node's `IrType`; a node that is
    /// neither `Int` nor `Long` answers a 32-bit top, which proves nothing and
    /// is what a caller that asked the wrong question deserves.
    pub fn of(&self, id: NodeId) -> Range {
        match self.ranges.get(id as usize) {
            Some(r) => *r,
            None => Range::top(IntWidth::W32),
        }
    }

    /// Whether the fixpoint settled. `false` means every entry is top: the
    /// analysis ran out of rounds and reported nothing rather than reporting a
    /// partial (and therefore possibly unsound) fixpoint.
    pub fn converged(&self) -> bool {
        self.converged
    }

    /// Number of nodes covered.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }
}

/// The lattice width a node's type computes at, or `None` when the node does
/// not carry an integer value.
fn width_of(ty: IrType) -> Option<IntWidth> {
    match ty {
        IrType::Int => Some(IntWidth::W32),
        IrType::Long => Some(IntWidth::W64),
        _ => None,
    }
}

/// Whether this op has a transfer function here. Everything else starts — and
/// stays — at top.
fn is_modelled(op: &Op) -> bool {
    matches!(
        op,
        Op::Const(_)
            | Op::Phi
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Cmp(_)
            | Op::LCmp
            | Op::FCmp { .. }
            | Op::ArrayLength
            | Op::I2L
            | Op::L2I
            | Op::I2B
            | Op::I2C
            | Op::I2S
            | Op::Load(_)
            | Op::ArrayLoad(_)
    )
}

/// Assign a [`Range`] to every `Int`/`Long`-typed node of `g`.
///
/// A bounded ascending fixpoint: every modelled node starts at bottom, every
/// unmodelled one at top, and each round recomputes the modelled nodes from
/// their inputs. φ nodes widen from [`WIDEN_AFTER_ROUND`] on so the walk
/// terminates.
///
/// **Fail-closed.** If the fixpoint has not settled by [`MAX_GRAPH_ROUNDS`],
/// every entry is reset to top and [`GraphRanges::converged`] answers `false`.
/// A partial fixpoint of an ascending analysis is *not* a sound
/// over-approximation — a node still sitting at bottom claims "this value
/// cannot occur" — so it must never be handed to a consumer.
pub fn analyze_graph(g: &Graph) -> GraphRanges {
    let n = g.nodes.len();
    let mut ranges: Vec<Range> = Vec::with_capacity(n);
    for node in &g.nodes {
        let w = width_of(node.ty).unwrap_or(IntWidth::W32);
        if width_of(node.ty).is_some() && is_modelled(&node.op) {
            ranges.push(Range::empty(w));
        } else {
            ranges.push(Range::top(w));
        }
    }

    let mut converged = false;
    for round in 0..MAX_GRAPH_ROUNDS {
        let mut changed = false;
        for id in 0..n {
            let node = &g.nodes[id];
            let Some(w) = width_of(node.ty) else { continue };
            if !is_modelled(&node.op) {
                continue;
            }
            let computed = transfer(g, &ranges, id, w);
            // Ascend only. The transfer function is monotone in its inputs and
            // the inputs only ever grow, so this join is normally the identity
            // — it is here so that a hypothetical non-monotone arm can never
            // break termination.
            let mut next = ranges[id].join(computed);
            if node.op == Op::Phi && round >= WIDEN_AFTER_ROUND {
                next = ranges[id].widen(next);
            }
            if next != ranges[id] {
                ranges[id] = next;
                changed = true;
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }

    if !converged {
        for r in ranges.iter_mut() {
            let w = r.width();
            *r = Range::top(w);
        }
    }
    GraphRanges { ranges, converged }
}

// ===========================================================================
// Path-sensitive refinement (B3)
// ===========================================================================

/// How many node re-evaluations the refinement may perform for one block
/// before it stops and leaves the rest of that block's nodes at their global
/// range.
///
/// Stopping early is always SOUND -- every entry in the overlay is a subset of
/// the global range, so dropping one only proves less -- which makes this a
/// compile-time bound and not a correctness one. It is per block rather than
/// per method so one enormous block cannot starve every later one.
const MAX_PATH_REFINEMENTS: usize = 256;

/// How many times the refinement sweeps outward from the pinned values.
///
/// Two covers the shapes this exists for (`a[i]` under `if (i >= 0)` is one
/// step, `a[i + 1]` under the same is two). A descending chain on a cyclic
/// sub-graph is bounded by [`MAX_PATH_REFINEMENTS`], not by this.
const PATH_REFINE_ROUNDS: usize = 2;

/// What the refinement did. For tests, and for a census line when one is
/// wanted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathCensus {
    /// Blocks entered through an `If` edge whose condition narrowed something.
    pub blocks_with_facts: usize,
    /// Values narrowed directly by a comparison, summed over those blocks.
    pub pinned: usize,
    /// Values narrowed by propagating a pinned fact through a transfer
    /// function.
    pub refined: usize,
    /// Blocks where [`MAX_PATH_REFINEMENTS`] ran out. Non-zero means the
    /// analysis proved less than it could have -- never that it proved
    /// something wrong.
    pub budget_exhausted: usize,
    /// `If` edges whose condition was a shape this pass does not read.
    pub unmodelled_conditions: usize,
}

/// [`analyze_graph`] plus, for each basic block, the narrowings implied by the
/// `If` edges that must have been taken to reach it.
///
/// # What makes this sound
///
/// A fact is recorded for block `b` only when `b` has exactly one predecessor
/// and its control node is a projection of an `Op::If`. Every path to `b` has
/// therefore evaluated that condition to that answer. Facts are then inherited
/// down the DOMINATOR tree, which extends them to exactly the blocks every
/// path to which runs through `b`.
///
/// [`PathRanges::at`] is the only query that sees them. [`PathRanges::of`]
/// still answers the whole-method range, so a caller with no block in hand
/// gets what it got before this existed.
pub struct PathRanges {
    base: GraphRanges,
    /// `narrowed[b][id]` is `id`'s range for a use in block `b`, present only
    /// when it is strictly below the global one.
    narrowed: Vec<rustc_hash::FxHashMap<NodeId, Range>>,
    census: PathCensus,
}

impl PathRanges {
    /// A [`PathRanges`] carrying NO edge facts, so [`PathRanges::at`] answers
    /// exactly [`PathRanges::of`] for every block.
    ///
    /// This is what `CRATONVM_JIT_IR_RANGE_EDGES=0` produces. Having the
    /// switch select a fact-free overlay rather than a different TYPE keeps one
    /// query path in the consumer: with two, the off arm is a second body that
    /// no test exercises, which is how a kill switch stops being a way back.
    pub fn flow_insensitive(base: GraphRanges) -> PathRanges {
        PathRanges {
            base,
            narrowed: Vec::new(),
            census: PathCensus::default(),
        }
    }

    /// The whole-method range of `id` -- exactly [`GraphRanges::of`].
    pub fn of(&self, id: NodeId) -> Range {
        self.base.of(id)
    }

    /// The range of `id` for a use in block `block`.
    ///
    /// Falls back to the whole-method range for an unknown block or an
    /// un-narrowed node, so any block index gets an answer that is correct if
    /// weaker.
    pub fn at(&self, id: NodeId, block: usize) -> Range {
        match self.narrowed.get(block).and_then(|m| m.get(&id)) {
            Some(r) => *r,
            None => self.base.of(id),
        }
    }

    /// Whether the underlying fixpoint settled. `false` means every range is
    /// top and no facts were recorded.
    pub fn converged(&self) -> bool {
        self.base.converged()
    }

    /// The flow-insensitive result this refines.
    pub fn base(&self) -> &GraphRanges {
        &self.base
    }

    /// What the refinement did.
    pub fn census(&self) -> PathCensus {
        self.census
    }
}

/// Run [`analyze_graph`] and refine it along `If` edges.
///
/// See [`PathRanges`] for the soundness argument. A non-converged fixpoint
/// records NO facts: every base range is top, narrowing top against top proves
/// nothing, and walking anyway would only cost time.
pub fn analyze_graph_pathwise(g: &Graph, schedule: &Schedule) -> PathRanges {
    let base = analyze_graph(g);
    let nblocks = schedule.blocks.len();
    let mut narrowed: Vec<rustc_hash::FxHashMap<NodeId, Range>> =
        vec![rustc_hash::FxHashMap::default(); nblocks];
    let mut census = PathCensus::default();

    if !base.converged() {
        return PathRanges {
            base,
            narrowed,
            census,
        };
    }

    // One user index for the whole graph. `Graph::users_of` falls back to a
    // full scan when the use lists are not current, which inside a per-node
    // loop would be quadratic; this is O(nodes + edges), once.
    let users = build_user_index(g);

    for b in dom_preorder(schedule) {
        // Inherit every fact of the immediate dominator. `dom_preorder` visits
        // a block only after its idom, so that entry is already final.
        if let Some(p) = schedule.dom.idom(b) {
            if p != b && p < nblocks && !narrowed[p].is_empty() {
                narrowed[b] = narrowed[p].clone();
            }
        }

        let Some((cond, taken)) = entry_edge_condition(g, schedule, b) else {
            continue;
        };

        let mut facts = std::mem::take(&mut narrowed[b]);
        let pinned = apply_condition(g, &base, &mut facts, cond, taken);
        if pinned.is_empty() {
            census.unmodelled_conditions += 1;
            narrowed[b] = facts;
            continue;
        }
        census.blocks_with_facts += 1;
        census.pinned += pinned.len();
        refine(g, &base, &users, &mut facts, &pinned, &mut census);
        narrowed[b] = facts;
    }

    PathRanges {
        base,
        narrowed,
        census,
    }
}

/// `index[i]` = the distinct nodes naming `i` as an input.
fn build_user_index(g: &Graph) -> Vec<Vec<NodeId>> {
    let mut index: Vec<Vec<NodeId>> = vec![Vec::new(); g.nodes.len()];
    for (uid, node) in g.nodes.iter().enumerate() {
        for &raw in node.inputs.as_slice() {
            if let Some(i) = node_id_opt(raw) {
                if let Some(slot) = index.get_mut(i as usize) {
                    if !slot.contains(&(uid as NodeId)) {
                        slot.push(uid as NodeId);
                    }
                }
            }
        }
    }
    index
}

/// Block indices, every block after its immediate dominator.
///
/// A block the entry does not reach has no idom and is visited as its own root
/// with no inherited facts, which is the right answer for code that cannot
/// run. `seen` is not merely an optimisation: a malformed idom array with a
/// cycle in it would otherwise spin here forever, and this pass must not be
/// able to hang the compiler thread on a graph it merely disagrees with.
fn dom_preorder(schedule: &Schedule) -> Vec<usize> {
    let n = schedule.blocks.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();
    for b in 0..n {
        match schedule.dom.idom(b) {
            Some(p) if p != b && p < n => children[p].push(b),
            _ => roots.push(b),
        }
    }
    let mut order = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut stack = roots;
    while let Some(b) = stack.pop() {
        if seen[b] {
            continue;
        }
        seen[b] = true;
        order.push(b);
        stack.extend(children[b].iter().copied());
    }
    order
}

/// The `(condition, was_taken)` of the single `If` edge block `b` is entered
/// through, or `None` when `b` is not entered that way.
///
/// The `Op::Proj` destructure is what actually keeps a fact off a block the
/// branch does not dominate: a `Merge` or `Region` block is headed by that node
/// and never by a projection, so it falls out here and inherits only what its
/// immediate dominator established. That is the path
/// `a_merge_of_both_arms_carries_no_fact_from_either` exercises.
///
/// The `predecessors.len() == 1` test is defence in depth and, on a well-formed
/// graph, unreachable: a projection has exactly one control input and nothing in
/// this IR can wire a second CFG edge into a block headed by one. Removing it
/// moves no test -- measured 2026-09-17, by deleting it and re-running both
/// modules. It is kept because it costs one comparison and because the thing it
/// refuses (a fact applied on a path that never evaluated the condition) deletes
/// bounds checks, but it is NOT the mechanism, and a future reader looking for
/// the guarantee should look at the line below rather than at this one.
fn entry_edge_condition(g: &Graph, schedule: &Schedule, b: usize) -> Option<(NodeId, bool)> {
    let block = schedule.blocks.get(b)?;
    if block.predecessors.len() != 1 {
        return None;
    }
    let ctrl = g.nodes.get(block.ctrl as usize)?;
    let Op::Proj(k) = ctrl.op else {
        return None;
    };
    if k > 1 {
        return None;
    }
    let parent = node_id_opt(*ctrl.inputs.first()?)?;
    if g.nodes.get(parent as usize)?.op != Op::If {
        return None;
    }
    let cond = node_id_opt(*g.nodes[parent as usize].inputs.get(1)?)?;
    // `Op::If` documents `Proj(0)` as the true edge and `Proj(1)` as the false
    // edge.
    Some((cond, k == 0))
}

/// Record the narrowings `cond == taken` implies, returning the nodes pinned.
///
/// A pinned node's range is a fact about the VALUE, not a computed result, so
/// [`refine`] must never recompute it from its inputs.
fn apply_condition(
    g: &Graph,
    base: &GraphRanges,
    facts: &mut rustc_hash::FxHashMap<NodeId, Range>,
    cond: NodeId,
    taken: bool,
) -> Vec<NodeId> {
    let Some(node) = g.nodes.get(cond as usize) else {
        return Vec::new();
    };
    let Op::Cmp(raw_op) = node.op else {
        return Vec::new();
    };
    // The false edge proves the NEGATION, not the mirror image. `negate` and
    // `swapped` are different functions, and `Eq`/`Ne` are fixed points of the
    // second, so a test built only on equality would not notice them confused.
    let op = if taken { raw_op } else { raw_op.negate() };

    let (Some(l), Some(r)) = (
        node.inputs.first().copied().and_then(node_id_opt),
        node.inputs.get(1).copied().and_then(node_id_opt),
    ) else {
        return Vec::new();
    };

    // `lcmp; if<cond> 0` -- the only way a `long` comparison reaches an `If`.
    // The `Cmp` is against the three-way result, so the fact belongs to
    // `LCmp`'s operands, and the operator carries over unchanged because
    // `lcmp(a, b) < 0` is exactly `a < b`.
    if matches!(g.nodes.get(l as usize).map(|n| &n.op), Some(Op::LCmp))
        && matches!(g.nodes.get(r as usize).map(|n| &n.op), Some(Op::Const(0)))
    {
        let lcmp = &g.nodes[l as usize];
        let (Some(a), Some(b)) = (
            lcmp.inputs.first().copied().and_then(node_id_opt),
            lcmp.inputs.get(1).copied().and_then(node_id_opt),
        ) else {
            return Vec::new();
        };
        return narrow_pair(g, base, facts, a, b, op);
    }

    narrow_pair(g, base, facts, l, r, op)
}

/// Narrow `l` and `r` by `l op r`, returning whichever actually moved.
fn narrow_pair(
    g: &Graph,
    base: &GraphRanges,
    facts: &mut rustc_hash::FxHashMap<NodeId, Range>,
    l: NodeId,
    r: NodeId,
    op: CmpOp,
) -> Vec<NodeId> {
    let (Some(wl), Some(wr)) = (
        g.nodes.get(l as usize).and_then(|n| width_of(n.ty)),
        g.nodes.get(r as usize).and_then(|n| width_of(n.ty)),
    ) else {
        return Vec::new();
    };
    if wl != wr {
        // Not comparable in this lattice; proving nothing is the safe answer.
        return Vec::new();
    }

    let cl = facts.get(&l).copied().unwrap_or_else(|| base.of(l));
    let cr = facts.get(&r).copied().unwrap_or_else(|| base.of(r));
    // Both sides narrow from the PRE-narrowing values. Feeding an
    // already-narrowed `l` into `r`'s computation would still be sound, but it
    // makes the result depend on which of the two is done first, and an
    // order-dependent analysis is one whose output moves when something
    // unrelated renumbers the graph.
    let nl = cl.narrow_by(op, cr);
    let nr = cr.narrow_by(op.swapped(), cl);

    let mut pinned = Vec::new();
    if nl != cl {
        facts.insert(l, nl);
        pinned.push(l);
    }
    if nr != cr && r != l {
        facts.insert(r, nr);
        pinned.push(r);
    }
    pinned
}

/// Propagate pinned facts through the transfer functions.
///
/// # Why `Op::Phi` is excluded -- conservatism, not a proven necessity
///
/// Every other modelled op is a PURE FUNCTION of its inputs, so if each input
/// lies in a narrowed range at this block, so does the result. That argument
/// does not carry to a phi, and the reason is worth stating precisely because
/// it is easy to talk yourself out of.
///
/// A phi's value is chosen by which predecessor ran. Across a LOOP the same SSA
/// node takes a different value per iteration, and the facts recorded here
/// describe the current one: at the loop body, a bound on `next` bounds
/// `next_k`, while the header phi's value is `next_{k-1}`. Joining `next`'s
/// refined range into the phi therefore reasons about one iteration with a fact
/// established in another. Whether that is actually unsound depends on the
/// facts being loop-invariant, which they are in every shape checked here --
/// so the honest statement is that the argument for allowing it is subtle and
/// the argument for refusing it is short.
///
/// **No test distinguishes the two.** Measured 2026-09-17: deleting this arm
/// and re-running `range_analysis` and `ir_check_elim` moved nothing, including
/// `a_loop_phi_keeps_a_lower_bound_that_admits_its_entry_value`, which was
/// written expecting to catch it. That test pins the OUTCOME the exclusion is
/// there to protect, not the exclusion itself. Anyone deleting this arm to buy
/// precision should know it is currently unpinned, and should bring the
/// counter-example that would have failed.
///
/// A phi still keeps whatever [`apply_condition`] pinned on it -- that is a
/// direct fact about the value at this block, and is sound without this
/// argument.
fn refine(
    g: &Graph,
    base: &GraphRanges,
    users: &[Vec<NodeId>],
    facts: &mut rustc_hash::FxHashMap<NodeId, Range>,
    pinned: &[NodeId],
    census: &mut PathCensus,
) {
    let mut budget = MAX_PATH_REFINEMENTS;
    let mut frontier: Vec<NodeId> = pinned.to_vec();

    for _round in 0..PATH_REFINE_ROUNDS {
        let mut next: Vec<NodeId> = Vec::new();
        for &changed in &frontier {
            let Some(list) = users.get(changed as usize) else {
                continue;
            };
            for &u in list {
                if budget == 0 {
                    census.budget_exhausted += 1;
                    return;
                }
                if pinned.contains(&u) {
                    continue;
                }
                let Some(node) = g.nodes.get(u as usize) else {
                    continue;
                };
                if node.op == Op::Phi || !is_modelled(&node.op) {
                    continue;
                }
                let Some(w) = width_of(node.ty) else {
                    continue;
                };
                budget -= 1;

                let computed = {
                    let view = |n: NodeId| -> Option<Range> {
                        if (n as usize) >= g.nodes.len() {
                            return None;
                        }
                        Some(facts.get(&n).copied().unwrap_or_else(|| base.of(n)))
                    };
                    transfer_via(g, u as usize, w, &view)
                };

                let curr = facts.get(&u).copied().unwrap_or_else(|| base.of(u));
                // Clamp to what is already known. The refinement may only ever
                // narrow: an overlay entry ABOVE the current answer would be a
                // second, weaker opinion about the same value, and `at` would
                // then disagree with `of` in the unsafe direction.
                let nextr = computed.meet(curr);
                if nextr != curr {
                    facts.insert(u, nextr);
                    census.refined += 1;
                    next.push(u);
                }
            }
        }
        if next.is_empty() {
            return;
        }
        frontier = next;
    }
}

/// How a transfer function looks up the range of an input node.
///
/// Introduced so the SAME transfer functions serve both the flow-insensitive
/// fixpoint (which reads a dense `&[Range]`) and the path-sensitive refinement
/// (which reads a sparse per-block overlay on top of it). Two copies of this
/// arithmetic that could drift apart is precisely the hazard this module's
/// header is about, so there is exactly one copy and the lookup is the
/// parameter.
type RangeLookup<'a> = dyn Fn(NodeId) -> Option<Range> + 'a;

/// The range of the value at input slot `slot` of node `id`, or top.
///
/// `w` is only the width of the answer when there is nothing to look up. A
/// node that IS found answers at its own width -- `I2L` reads a 32-bit input
/// and produces a 64-bit result, and flattening that to `w` here would hand
/// the `i2l` arm a range whose width contradicts its value.
fn input_range_via(
    g: &Graph,
    view: &RangeLookup<'_>,
    id: usize,
    slot: usize,
    w: IntWidth,
) -> Range {
    let Some(raw) = g.nodes[id].inputs.get(slot).copied() else {
        return Range::top(w);
    };
    let Some(idx) = node_id_opt(raw) else {
        return Range::top(w);
    };
    match view(idx) {
        Some(r) => r,
        None => Range::top(w),
    }
}

/// One node's transfer function, reading the dense fixpoint vector.
fn transfer(g: &Graph, ranges: &[Range], id: usize, w: IntWidth) -> Range {
    transfer_via(g, id, w, &|n: NodeId| ranges.get(n as usize).copied())
}

/// One node's transfer function, reading whatever `view` answers.
fn transfer_via(g: &Graph, id: usize, w: IntWidth, view: &RangeLookup<'_>) -> Range {
    let node = &g.nodes[id];
    let a = |slot: usize| input_range_via(g, view, id, slot, w);
    // A shift count is always an `int`, even for a `long` shiftee.
    let count = |slot: usize| input_range_via(g, view, id, slot, IntWidth::W32);
    match &node.op {
        // `Range::constant` CLAMPS into the width, which is the right answer
        // for a claim about a value already known to be representable and the
        // wrong one here: an `Int`-typed `Const(2147483648)` (an un-wrapped
        // fold, or a zero-extended `0xFFFF_FFFF`) is not `Integer.MAX_VALUE`
        // at runtime, it is whatever the 32-bit truncation makes it. Every
        // producer is supposed to store `int` constants sign-extended; one
        // that does not must cost precision here, not a clamped lie that a
        // bounds proof then trusts.
        Op::Const(v) => {
            if *v < w.min() || *v > w.max() {
                Range::top(w)
            } else {
                Range::constant(w, *v)
            }
        }
        Op::Phi => {
            let mut acc = Range::empty(w);
            for inp in node.phi_value_inputs() {
                acc = match inp.and_then(|i| view(i)) {
                    Some(r) => acc.join(r),
                    None => acc.join(Range::top(w)),
                };
            }
            acc
        }
        Op::Add => a(0).add(a(1)),
        Op::Sub => a(0).sub(a(1)),
        Op::Mul => a(0).mul(a(1)),
        Op::Div => a(0).div(a(1)),
        Op::Rem => a(0).rem(a(1)),
        Op::Neg => a(0).neg(),
        Op::And => a(0).and(a(1)),
        Op::Or => a(0).or(a(1)),
        Op::Xor => a(0).xor(a(1)),
        Op::Shl => a(0).shl(count(1)),
        Op::Shr => a(0).shr(count(1)),
        Op::UShr => a(0).ushr(count(1)),
        // A comparison materialises a boolean.
        Op::Cmp(_) => Range::exact(IntWidth::W32, 0, 1),
        // The three-way compares yield exactly {-1, 0, 1}.
        Op::LCmp | Op::FCmp { .. } => Range::exact(IntWidth::W32, -1, 1),
        Op::ArrayLength => Range::array_length(),
        // Conversions: the shiftee is at slot 0 but its width is the SOURCE
        // width, so it must be read at that width rather than at `w`.
        Op::I2L => input_range_via(g, view, id, 0, IntWidth::W32).i2l(),
        Op::L2I => input_range_via(g, view, id, 0, IntWidth::W64).l2i(),
        Op::I2B => input_range_via(g, view, id, 0, IntWidth::W32).i2b(),
        Op::I2C => input_range_via(g, view, id, 0, IntWidth::W32).i2c(),
        Op::I2S => input_range_via(g, view, id, 0, IntWidth::W32).i2s(),
        // A sub-word load or array element load is range-narrowing by its
        // element type; the wider kinds narrow nothing.
        Op::Load(kind) | Op::ArrayLoad(kind) => {
            Range::of_mem_kind(*kind).unwrap_or_else(|| Range::top(w))
        }
        _ => Range::top(w),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Inputs, UseLists};

    const W: IntWidth = IntWidth::W32;
    const L: IntWidth = IntWidth::W64;

    fn r(lo: i64, hi: i64) -> Range {
        Range::exact(W, lo, hi)
    }

    // ---- the overflow argument, first ------------------------------------

    /// **The mistake this module exists to prevent.** In mathematical
    /// integers `i + 1 > i`. On a 32-bit machine integer it is false exactly
    /// once, at `Integer.MAX_VALUE`, and an analysis that misses that deletes
    /// a real bounds check.
    ///
    /// The lattice must therefore answer `top` — not `[MIN+1, MAX+1]`, and not
    /// a silently clamped `[MIN+1, MAX]` — for `unknown_int + 1`.
    #[test]
    fn add_one_to_top_is_top_not_a_shifted_interval() {
        let top = Range::top(W);
        let sum = top.add(Range::constant(W, 1));
        assert!(sum.is_top(), "unknown + 1 must be top, got {sum:?}");
        assert!(
            sum.contains_all(Range::constant(W, i32::MIN as i64)),
            "the wrapped result IS Integer.MIN_VALUE; the range must admit it"
        );
        // And the exact form refuses rather than degrading.
        assert_eq!(top.add_no_wrap(Range::constant(W, 1)), None);
    }

    /// The same trap one step further out: a range that *ends* at
    /// `Integer.MAX_VALUE` still wraps.
    #[test]
    fn add_wraps_at_the_upper_endpoint_only() {
        // [0, MAX-1] + 1 = [1, MAX] — representable, so exact.
        let ok = r(0, i32::MAX as i64 - 1).add_no_wrap(Range::constant(W, 1));
        assert_eq!(ok, Some(r(1, i32::MAX as i64)));
        // [0, MAX] + 1 is not.
        assert_eq!(
            r(0, i32::MAX as i64).add_no_wrap(Range::constant(W, 1)),
            None
        );
        assert!(r(0, i32::MAX as i64).add(Range::constant(W, 1)).is_top());
    }

    /// `-Integer.MIN_VALUE` is `Integer.MIN_VALUE`, not `2^31`.
    #[test]
    fn neg_of_min_wraps() {
        assert_eq!(Range::constant(W, i32::MIN as i64).neg_no_wrap(), None);
        assert!(Range::constant(W, i32::MIN as i64).neg().is_top());
        assert_eq!(r(-5, 5).neg(), r(-5, 5));
        assert_eq!(r(1, 5).neg(), r(-5, -1));
    }

    /// A 64-bit range is not an `int` range, and the two must never be mixed
    /// silently: a cross-width join degrades to top rather than producing an
    /// interval in the wrong domain.
    #[test]
    fn width_mismatch_degrades_to_top() {
        let i = r(0, 10);
        let l = Range::exact(L, 0, 10);
        assert!(i.join(l).is_top());
        assert!(i.add(l).is_top());
        assert_eq!(i.meet(l), i, "meet cannot narrow across widths");
        assert!(!i.contains_all(l));
        // 64-bit ranges do not convert into the `int`-only scev lattice.
        assert_eq!(l.to_scev(), None);
    }

    // ---- lattice laws ----------------------------------------------------

    #[test]
    fn join_is_commutative_associative_and_absorbing() {
        let a = r(-3, 4);
        let b = r(2, 9);
        let c = r(-100, -50);
        assert_eq!(a.join(b), b.join(a));
        assert_eq!(a.join(b).join(c), a.join(b.join(c)));
        assert_eq!(a.join(a), a);
        assert_eq!(a.join(Range::empty(W)), a);
        assert!(a.join(Range::top(W)).is_top());
        assert_eq!(a.join(b), r(-3, 9));
    }

    #[test]
    fn meet_is_commutative_associative_and_can_contradict() {
        let a = r(-3, 4);
        let b = r(2, 9);
        assert_eq!(a.meet(b), b.meet(a));
        assert_eq!(a.meet(b), r(2, 4));
        assert_eq!(a.meet(a), a);
        assert_eq!(a.meet(Range::top(W)), a);
        assert!(a.meet(Range::empty(W)).is_empty());
        // A genuine contradiction is bottom, never a pick-one-side.
        assert!(r(0, 3).meet(r(7, 9)).is_empty());
    }

    /// Monotonicity of join and meet, checked exhaustively over a small grid.
    /// `a ⊑ a'` must imply `a ⊔ b ⊑ a' ⊔ b` and `a ⊓ b ⊑ a' ⊓ b`.
    #[test]
    fn join_and_meet_are_monotone() {
        let pts: Vec<Range> = {
            let mut v = vec![Range::empty(W)];
            for lo in -3..=3i64 {
                for hi in lo..=3i64 {
                    v.push(r(lo, hi));
                }
            }
            v
        };
        for a in &pts {
            for ap in &pts {
                if !ap.contains_all(*a) {
                    continue; // only pairs with a ⊑ a'
                }
                for b in &pts {
                    assert!(
                        ap.join(*b).contains_all(a.join(*b)),
                        "join not monotone: {a:?} ⊑ {ap:?} but {:?} ⋢ {:?}",
                        a.join(*b),
                        ap.join(*b)
                    );
                    assert!(
                        ap.meet(*b).contains_all(a.meet(*b)),
                        "meet not monotone: {a:?} ⊑ {ap:?} but {:?} ⋢ {:?}",
                        a.meet(*b),
                        ap.meet(*b)
                    );
                }
            }
        }
    }

    /// Arithmetic must be monotone too, or the fixpoint's "inputs only grow"
    /// termination argument is worthless.
    #[test]
    fn add_is_monotone_and_over_approximates_pointwise() {
        let small = r(0, 2);
        let big = r(-1, 5);
        assert!(big.contains_all(small));
        assert!(big.add(r(1, 1)).contains_all(small.add(r(1, 1))));
        // Pointwise soundness: every concrete sum is admitted.
        let sum = r(3, 7).add(r(-2, 4));
        for x in 3..=7i64 {
            for y in -2..=4i64 {
                assert!(
                    sum.contains_all(Range::constant(W, x + y)),
                    "{x} + {y} not admitted by {sum:?}"
                );
            }
        }
    }

    #[test]
    fn widen_throws_outward_endpoints_to_the_extremes() {
        let prev = r(0, 10);
        // Growing upward only: the lower endpoint stays put.
        let w1 = prev.widen(r(0, 11));
        assert_eq!(w1, r(0, i32::MAX as i64));
        // Growing downward only.
        let w2 = prev.widen(r(-1, 10));
        assert_eq!(w2, r(i32::MIN as i64, 10));
        // Not growing at all: unchanged.
        assert_eq!(prev.widen(r(2, 8)), prev);
        // Bottom is the identity in both directions.
        assert_eq!(Range::empty(W).widen(prev), prev);
        assert_eq!(prev.widen(Range::empty(W)), prev);
    }

    /// Widening must saturate: repeatedly widening against a range that keeps
    /// growing has to reach top in a bounded number of steps, or the fixpoint
    /// never terminates.
    #[test]
    fn widen_saturates_in_bounded_steps() {
        let mut acc = r(0, 0);
        let mut steps = 0;
        for i in 1..100i64 {
            let next = acc.join(r(-i, i));
            let widened = acc.widen(next);
            steps += 1;
            if widened == acc {
                break;
            }
            acc = widened;
            if acc.is_top() {
                break;
            }
        }
        assert!(acc.is_top(), "widening did not saturate, ended at {acc:?}");
        assert!(steps <= 4, "widening took {steps} steps to saturate");
    }

    // ---- the narrowing transfer functions --------------------------------

    /// `hash & (n - 1)`: the mask is non-negative, so the result is in
    /// `[0, mask]` no matter how wild the hash is. This is the fact that makes
    /// a hash-table index provable.
    #[test]
    fn and_with_a_non_negative_mask_bounds_the_result() {
        let hash = Range::top(W);
        assert_eq!(hash.and(Range::constant(W, 15)), r(0, 15));
        assert_eq!(Range::constant(W, 15).and(hash), r(0, 15));
        // Both non-negative: the tighter of the two upper bounds wins.
        assert_eq!(r(0, 100).and(r(0, 7)), r(0, 7));
        // Both possibly negative: nothing is known.
        assert!(hash.and(Range::top(W)).is_top());
        // Both entirely negative: the sign bit survives.
        let neg = r(i32::MIN as i64, -1);
        assert_eq!(neg.and(neg), neg);
    }

    #[test]
    fn or_and_xor_are_modelled_only_for_non_negative_operands() {
        assert_eq!(r(0, 5).or(r(0, 9)), r(0, 15));
        assert_eq!(r(4, 5).or(r(2, 3)), r(4, 7));
        assert_eq!(r(0, 5).xor(r(0, 9)), r(0, 15));
        assert!(r(-1, 5).or(r(0, 9)).is_top());
        assert!(r(-1, 5).xor(r(0, 9)).is_top());
    }

    #[test]
    fn shifts_respect_the_jvms_count_mask() {
        // `x << 32` shifts an int by 0, not by 32.
        assert_eq!(r(1, 1).shl(Range::constant(W, 32)), r(1, 1));
        assert_eq!(r(1, 1).shl(Range::constant(W, 33)), r(2, 2));
        // A long shifts by the low 6 bits.
        let l1 = Range::constant(L, 1);
        assert_eq!(l1.shl(Range::constant(W, 64)), l1);
        assert_eq!(l1.shl(Range::constant(W, 65)), Range::constant(L, 2));
        // A shift that overflows the width wraps: top.
        assert!(r(1, 1).shl(Range::constant(W, 31)).is_top());
        // An unknown count proves nothing about a left shift.
        assert!(r(1, 1).shl(Range::top(W)).is_top());
    }

    #[test]
    fn arithmetic_shift_right_never_wraps() {
        assert_eq!(r(-16, 16).shr(Range::constant(W, 2)), r(-4, 4));
        assert_eq!(r(-1, -1).shr(Range::constant(W, 31)), r(-1, -1));
        // Unknown count: the value can only move toward 0 / -1.
        assert_eq!(r(5, 40).shr(Range::top(W)), r(0, 40));
        assert_eq!(r(-40, -5).shr(Range::top(W)), r(-40, -1));
        assert_eq!(r(-40, 40).shr(Range::top(W)), r(-40, 40));
    }

    #[test]
    fn unsigned_shift_right_of_a_negative_value_is_non_negative() {
        // (-1 >>> 1) == 0x7FFFFFFF
        assert_eq!(
            Range::constant(W, -1).ushr(Range::constant(W, 1)),
            r(0, i32::MAX as i64)
        );
        // A non-negative shiftee keeps its exact bounds.
        assert_eq!(r(8, 16).ushr(Range::constant(W, 3)), r(1, 2));
        // Count 0 is the identity — including for a negative value, which is
        // why an unknown count must be top.
        assert_eq!(
            Range::constant(W, -1).ushr(Range::constant(W, 0)),
            Range::constant(W, -1)
        );
        assert!(Range::constant(W, -1).ushr(Range::top(W)).is_top());
    }

    #[test]
    fn rem_follows_the_dividend_sign_and_bounds_by_the_divisor() {
        // A non-negative dividend gives a non-negative remainder.
        assert_eq!(Range::top(W).rem(Range::constant(W, 8)), r(-7, 7));
        assert_eq!(r(0, 1000).rem(Range::constant(W, 8)), r(0, 7));
        // `Integer.MIN_VALUE % -1` is 0 in Java, not an overflow.
        assert_eq!(
            Range::constant(W, i32::MIN as i64).rem(Range::constant(W, -1)),
            r(0, 0)
        );
        // Division by a divisor that can only be zero never produces a value.
        assert!(r(0, 5).rem(Range::constant(W, 0)).is_empty());
        // |a % b| <= |a| too.
        assert_eq!(r(0, 3).rem(Range::constant(W, 100)), r(0, 3));
    }

    #[test]
    fn div_by_a_constant_is_exact_and_min_over_minus_one_wraps() {
        assert_eq!(r(-10, 10).div(Range::constant(W, 3)), r(-3, 3));
        assert_eq!(r(-10, 10).div(Range::constant(W, -3)), r(-3, 3));
        assert!(Range::constant(W, i32::MIN as i64)
            .div(Range::constant(W, -1))
            .is_top());
        assert!(r(0, 5).div(Range::constant(W, 0)).is_empty());
    }

    #[test]
    fn conversions_truncate_rather_than_intersect() {
        // Already inside the target type: exact.
        assert_eq!(r(0, 100).i2b(), r(0, 100));
        // Outside it: the whole target type, NOT an intersection (`300 as byte`
        // is 44, which an intersection would have wrongly excluded).
        assert_eq!(r(0, 300).i2b(), r(-128, 127));
        assert!(r(0, 300).i2b().contains_all(Range::constant(W, 44)));
        assert_eq!(r(0, 300).i2c(), r(0, 300));
        assert_eq!(r(-1, 5).i2c(), r(0, 65535));
        assert_eq!(r(-1, 5).i2s(), r(-1, 5));
        // i2l is exact; l2i truncates.
        assert_eq!(r(-5, 5).i2l(), Range::exact(L, -5, 5));
        assert_eq!(Range::exact(L, -5, 5).l2i(), r(-5, 5));
        assert!(Range::exact(L, 0, 1i64 << 40).l2i().is_top());
    }

    /// Bottom is not non-negative, whatever the logic of vacuous truth says.
    ///
    /// The one property `definitely_non_negative` exists for: a consumer turns
    /// "every value is >= 0" into a deleted bounds check, and at bottom there
    /// is no value to have been checked. The vacuous predicate this replaces
    /// answered `true` here.
    #[test]
    fn bottom_is_not_definitely_non_negative() {
        assert!(!Range::empty(W).definitely_non_negative());
        assert!(Range::empty(W).is_empty());
        // …while a range that admits values and starts at or above zero is.
        assert!(r(0, 0).definitely_non_negative());
        assert!(r(1, 7).definitely_non_negative());
        assert!(!r(-1, 7).definitely_non_negative());
        assert!(!Range::top(W).definitely_non_negative());
    }

    #[test]
    fn array_length_and_mem_kinds() {
        assert_eq!(Range::array_length(), r(0, i32::MAX as i64));
        assert!(Range::array_length().definitely_non_negative());
        assert_eq!(Range::of_mem_kind(MemKind::Byte), Some(r(-128, 127)));
        assert_eq!(Range::of_mem_kind(MemKind::Char), Some(r(0, 65535)));
        assert_eq!(Range::of_mem_kind(MemKind::Short), Some(r(-32768, 32767)));
        assert_eq!(Range::of_mem_kind(MemKind::Int), None);
        assert_eq!(Range::of_mem_kind(MemKind::Ref), None);
    }

    #[test]
    fn scev_conversion_round_trips() {
        let a = r(-7, 19);
        let s = a.to_scev().expect("32-bit range converts");
        assert_eq!(Range::from_scev(s), a);
        assert!(Range::from_scev(IntRange::unknown()).is_top());
        assert!(Range::from_scev(IntRange::empty()).is_empty());
        assert_eq!(
            Range::array_length().to_scev(),
            Some(IntRange::array_length())
        );
    }

    // ---- the sea-of-nodes evaluator --------------------------------------

    fn empty_graph() -> Graph {
        Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
            uses: UseLists::default(),
            receiver_param: None,
        }
    }

    fn push(g: &mut Graph, op: Op, ty: IrType, inputs: Vec<NodeId>) -> NodeId {
        // Cast: dense ids assigned from 0, far below `NO_NODE`.
        let id = g.nodes.len() as NodeId;
        g.nodes.push(crate::ir::Node {
            op,
            ty,
            inputs: Inputs::from(inputs),
            bytecode_pc: None,
            frame_snapshot: None,
            volatile_access: false,
        });
        id
    }

    #[test]
    fn graph_folds_constants_and_narrows_array_length() {
        let mut g = empty_graph();
        let c3 = push(&mut g, Op::Const(3), IrType::Int, vec![]);
        let c4 = push(&mut g, Op::Const(4), IrType::Int, vec![]);
        let sum = push(&mut g, Op::Add, IrType::Int, vec![c3, c4]);
        let len = push(&mut g, Op::ArrayLength, IrType::Int, vec![]);
        let masked = push(&mut g, Op::And, IrType::Int, vec![len, c3]);
        let ranges = analyze_graph(&g);
        assert!(ranges.converged());
        assert_eq!(ranges.of(sum).as_constant(), Some(7));
        assert_eq!(ranges.of(len), r(0, i32::MAX as i64));
        assert_eq!(ranges.of(masked), r(0, 3));
    }

    #[test]
    fn graph_leaves_unmodelled_producers_at_top() {
        let mut g = empty_graph();
        let p = push(&mut g, Op::Param(0), IrType::Int, vec![]);
        let c1 = push(&mut g, Op::Const(1), IrType::Int, vec![]);
        let sum = push(&mut g, Op::Add, IrType::Int, vec![p, c1]);
        let ranges = analyze_graph(&g);
        assert!(ranges.converged());
        assert!(ranges.of(p).is_top(), "a parameter is an unknown");
        assert!(
            ranges.of(sum).is_top(),
            "unknown + 1 stays unknown — the wrap case again"
        );
    }

    #[test]
    fn graph_narrows_a_byte_array_load() {
        let mut g = empty_graph();
        let load = push(&mut g, Op::ArrayLoad(MemKind::Byte), IrType::Int, vec![]);
        let cload = push(&mut g, Op::Load(MemKind::Char), IrType::Int, vec![]);
        let iload = push(&mut g, Op::ArrayLoad(MemKind::Int), IrType::Int, vec![]);
        let ranges = analyze_graph(&g);
        assert_eq!(ranges.of(load), r(-128, 127));
        assert_eq!(ranges.of(cload), r(0, 65535));
        assert!(ranges.of(iload).is_top());
    }

    /// A φ whose inputs disagree is their hull; a φ feeding itself through an
    /// increment is the cyclic case widening exists for. The point of the test
    /// is that it **terminates** and lands on a sound answer, not that the
    /// answer is tight.
    #[test]
    fn graph_phi_cycle_terminates_and_stays_sound() {
        let mut g = empty_graph();
        let region = push(&mut g, Op::Region, IrType::Control, vec![]);
        let zero = push(&mut g, Op::Const(0), IrType::Int, vec![]);
        let one = push(&mut g, Op::Const(1), IrType::Int, vec![]);
        // phi = φ(0, phi + 1) — the id of the Add is not known yet, so patch
        // the edge after both exist.
        let phi = push(&mut g, Op::Phi, IrType::Int, vec![region, zero, zero]);
        let inc = push(&mut g, Op::Add, IrType::Int, vec![phi, one]);
        g.nodes[phi as usize].inputs = Inputs::from(vec![region, zero, inc]);

        let ranges = analyze_graph(&g);
        assert!(ranges.converged(), "the widened φ cycle must settle");
        let p = ranges.of(phi);
        assert!(
            p.contains_all(Range::constant(W, 0)),
            "0 is the entry value and must be admitted, got {p:?}"
        );
        assert!(
            p.contains_all(Range::constant(W, 1)),
            "1 is reachable after one increment, got {p:?}"
        );
        // Soundness demands the wrapped value be admitted too, because nothing
        // here bounds the trip count. Widening delivers exactly that.
        assert!(
            p.contains_all(Range::constant(W, i32::MIN as i64)),
            "an unbounded counter wraps; the range must admit it, got {p:?}"
        );
    }

    /// A φ with a merge edge only (no value inputs) and a φ with a dangling
    /// value edge both have to answer something sound rather than panic.
    #[test]
    fn graph_tolerates_malformed_phis() {
        let mut g = empty_graph();
        let region = push(&mut g, Op::Region, IrType::Control, vec![]);
        let bare = push(&mut g, Op::Phi, IrType::Int, vec![region]);
        let dangling = push(
            &mut g,
            Op::Phi,
            IrType::Int,
            vec![region, crate::ir::NO_NODE],
        );
        let ranges = analyze_graph(&g);
        assert!(ranges.of(bare).is_empty(), "no inputs means no values");
        assert!(ranges.of(dangling).is_top(), "an unknown edge is unknown");
    }

    #[test]
    fn graph_out_of_range_query_is_top() {
        let g = empty_graph();
        let ranges = analyze_graph(&g);
        assert_eq!(ranges.len(), 0);
        assert!(ranges.of(7).is_top());
    }

    // ---- B3: the narrowing primitives ------------------------------------

    /// The plus/minus one at the extremes of the width.
    ///
    /// `x < [MIN, MIN]` has no solution. Saying so must not be computed by
    /// evaluating `MIN - 1`, which in a release build wraps silently to `MAX`
    /// and inverts the bound into "x can be anything" -- the one arithmetic
    /// slip in this family that turns a refusal into permission.
    #[test]
    fn narrowing_at_the_width_extremes_does_not_wrap() {
        for w in [W, L] {
            let bottom = Range::constant(w, w.min());
            let top_v = Range::constant(w, w.max());

            assert!(
                Range::top(w).narrow_lt(bottom).is_empty(),
                "nothing is below the smallest representable value ({w:?})",
            );
            assert!(
                Range::top(w).narrow_gt(top_v).is_empty(),
                "nothing is above the largest representable value ({w:?})",
            );
            assert_eq!(
                Range::top(w).narrow_le(bottom),
                bottom,
                "<= MIN leaves exactly MIN ({w:?})",
            );
            assert_eq!(
                Range::top(w).narrow_ge(top_v),
                top_v,
                ">= MAX leaves exactly MAX ({w:?})",
            );
        }
    }

    /// The direction rule: a bound is read off the FAR endpoint of the other
    /// operand.
    ///
    /// `x < [3, 9]` admits `x == 8`, because the comparison only has to hold
    /// for SOME value the right-hand side can take. Reading the near endpoint
    /// would give `[MIN, 2]` and drop five values that genuinely occur -- the
    /// unsound direction, and the one that reads as a tighter, better analysis.
    #[test]
    fn a_bound_comes_from_the_far_endpoint_of_the_other_operand() {
        let x = Range::top(W);
        let other = r(3, 9);
        assert_eq!(x.narrow_lt(other), Range::exact(W, i32::MIN as i64, 8));
        assert_eq!(x.narrow_le(other), Range::exact(W, i32::MIN as i64, 9));
        assert_eq!(x.narrow_gt(other), Range::exact(W, 4, i32::MAX as i64));
        assert_eq!(x.narrow_ge(other), Range::exact(W, 3, i32::MAX as i64));
    }

    /// An interval lattice has no hole, so `!=` proves something only against a
    /// constant sitting on an endpoint.
    #[test]
    fn inequality_only_moves_an_endpoint() {
        assert_eq!(r(0, 10).narrow_ne(Range::constant(W, 0)), r(1, 10));
        assert_eq!(r(0, 10).narrow_ne(Range::constant(W, 10)), r(0, 9));
        // A value in the middle cannot be excised.
        assert_eq!(r(0, 10).narrow_ne(Range::constant(W, 5)), r(0, 10));
        // A non-constant right-hand side proves nothing at all.
        assert_eq!(r(0, 10).narrow_ne(r(0, 0).join(r(3, 3))), r(0, 10));
        // The singleton that excludes itself is bottom, not the same singleton.
        assert!(r(7, 7).narrow_ne(Range::constant(W, 7)).is_empty());
    }

    /// Widths are not comparable across the lattice, and refusing to narrow is
    /// the safe answer.
    #[test]
    fn a_width_mismatch_narrows_nothing() {
        let i = Range::top(W);
        let l = Range::constant(L, 0);
        assert_eq!(i.narrow_lt(l), i);
        assert_eq!(i.narrow_ge(l), i);
        assert_eq!(i.narrow_by(CmpOp::Gt, l), i);
    }

    /// `negate` and `swapped` are different functions, pinned here because
    /// `apply_condition` picks between them and `Eq`/`Ne` are fixed points of
    /// the second -- a test on equality alone would not notice them confused.
    #[test]
    fn negate_and_swapped_are_not_the_same_function() {
        assert_eq!(CmpOp::Lt.negate(), CmpOp::Ge);
        assert_eq!(CmpOp::Lt.swapped(), CmpOp::Gt);
        assert_eq!(CmpOp::Le.negate(), CmpOp::Gt);
        assert_eq!(CmpOp::Le.swapped(), CmpOp::Ge);
        for op in [CmpOp::Eq, CmpOp::Ne] {
            assert_eq!(op.swapped(), op, "{op:?} is its own mirror");
            assert_ne!(op.negate(), op, "{op:?} is not its own negation");
        }
        // And the property that makes `swapped` correct at all.
        for op in [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ] {
            assert_eq!(op.swapped().swapped(), op, "{op:?} mirrors back");
            assert_eq!(op.negate().negate(), op, "{op:?} negates back");
        }
    }

    // ---- B3: the graph walk ----------------------------------------------

    /// `if (p >= 0) { use p }`, scheduled, then asked at each block.
    ///
    /// Returns `(graph, schedule, param, true_block, false_block)`.
    fn sign_guarded_graph() -> (Graph, Schedule, NodeId, usize, usize) {
        let mut g = empty_graph();
        let start = push(&mut g, Op::Start, IrType::Control, vec![]);
        g.entry = start;
        let c0 = push(&mut g, Op::Proj(0), IrType::Control, vec![start]);
        let p = push(&mut g, Op::Param(0), IrType::Int, vec![start]);
        let zero = push(&mut g, Op::Const(0), IrType::Int, vec![]);
        let cmp = push(&mut g, Op::Cmp(CmpOp::Ge), IrType::Int, vec![p, zero]);
        let iff = push(&mut g, Op::If, IrType::Control, vec![c0, cmp]);
        let t = push(&mut g, Op::Proj(0), IrType::Control, vec![iff]);
        let f = push(&mut g, Op::Proj(1), IrType::Control, vec![iff]);
        g.exit = push(&mut g, Op::Return, IrType::Void, vec![t, p]);
        let sched = crate::ir_schedule::schedule(&g);
        let tb = sched.node_to_block[t as usize];
        let fb = sched.node_to_block[f as usize];
        (g, sched, p, tb, fb)
    }

    /// The headline: a value with no shape at all is narrowed by the edge.
    #[test]
    fn an_if_edge_narrows_its_operand_in_the_block_it_guards() {
        let (g, sched, p, tb, fb) = sign_guarded_graph();
        let pr = analyze_graph_pathwise(&g, &sched);
        assert!(pr.converged());

        assert!(
            pr.of(p).is_top(),
            "the whole-method answer for a bare Param is still top -- the \
             refinement must not leak back into it",
        );
        assert!(
            pr.at(p, tb).definitely_non_negative(),
            "on the true edge of `p >= 0`, p is non-negative",
        );
        assert_eq!(
            pr.at(p, fb),
            Range::exact(W, i32::MIN as i64, -1),
            "and on the false edge it is negative",
        );
        assert_eq!(pr.census().blocks_with_facts, 2);
    }

    /// An unknown block index, and a node nothing narrowed, both fall back to
    /// the whole-method answer rather than to something invented.
    #[test]
    fn an_unknown_block_answers_the_whole_method_range() {
        let (g, sched, p, _tb, _fb) = sign_guarded_graph();
        let pr = analyze_graph_pathwise(&g, &sched);
        assert_eq!(pr.at(p, 9_999), pr.of(p));
        assert_eq!(pr.at(12_345, 0), pr.of(12_345));
    }

    /// A block a branch does not dominate must carry no fact from it. Here the
    /// two arms of an `if` merge, and at the merge `p` can be anything again --
    /// both a negative and a non-negative value reach it.
    ///
    /// The mechanism is `entry_edge_condition`'s `Op::Proj` destructure: the
    /// merge block is headed by `Op::Merge`, so it never reaches the fact-
    /// recording path at all. It is NOT the `predecessors.len() == 1` test --
    /// deleting that leaves this test passing, which is recorded at that
    /// function rather than left for the next person to rediscover.
    #[test]
    fn a_merge_of_both_arms_carries_no_fact_from_either() {
        let mut g = empty_graph();
        let start = push(&mut g, Op::Start, IrType::Control, vec![]);
        g.entry = start;
        let c0 = push(&mut g, Op::Proj(0), IrType::Control, vec![start]);
        let p = push(&mut g, Op::Param(0), IrType::Int, vec![start]);
        let zero = push(&mut g, Op::Const(0), IrType::Int, vec![]);
        let cmp = push(&mut g, Op::Cmp(CmpOp::Ge), IrType::Int, vec![p, zero]);
        let iff = push(&mut g, Op::If, IrType::Control, vec![c0, cmp]);
        let t = push(&mut g, Op::Proj(0), IrType::Control, vec![iff]);
        let f = push(&mut g, Op::Proj(1), IrType::Control, vec![iff]);
        let m = push(&mut g, Op::Merge, IrType::Control, vec![t, f]);
        g.exit = push(&mut g, Op::Return, IrType::Void, vec![m, p]);
        let sched = crate::ir_schedule::schedule(&g);
        let mb = sched.node_to_block[m as usize];

        let pr = analyze_graph_pathwise(&g, &sched);
        assert!(
            pr.at(p, mb).is_top(),
            "at a merge of both arms p is unconstrained again, got {:?}",
            pr.at(p, mb),
        );
    }

    /// A loop counter that starts negative must not be reported non-negative
    /// inside the body.
    ///
    /// `for (i = -5; i < n; i++)`: the body runs with `i == -5`, and the only
    /// thing `i < n` proves about `i` there is an UPPER bound. Its lower bound
    /// must still admit `-5`, or the consumer deletes a bounds check the first
    /// iteration needs.
    ///
    /// This pins the outcome, and it is the outcome `refine`'s phi exclusion
    /// exists to protect -- but it does NOT pin that exclusion: removing the
    /// exclusion leaves this passing, because recomputing the phi joins its
    /// `init` input back in and `-5` survives anyway. Said plainly here because
    /// the test was written expecting to catch that, and an A/B showed it does
    /// not.
    #[test]
    fn a_loop_phi_keeps_a_lower_bound_that_admits_its_entry_value() {
        let mut g = empty_graph();
        let start = push(&mut g, Op::Start, IrType::Control, vec![]);
        g.entry = start;
        let c0 = push(&mut g, Op::Proj(0), IrType::Control, vec![start]);
        let n = push(&mut g, Op::Param(0), IrType::Int, vec![start]);
        let init = push(&mut g, Op::Const(-5), IrType::Int, vec![]);
        let one = push(&mut g, Op::Const(1), IrType::Int, vec![]);
        // Region(entry, backedge); Phi(region, init, next)
        let region = push(&mut g, Op::Region, IrType::Control, vec![c0, c0]);
        let phi = push(&mut g, Op::Phi, IrType::Int, vec![region, init, 0]);
        let next = push(&mut g, Op::Add, IrType::Int, vec![phi, one]);
        g.nodes[phi as usize].inputs = Inputs::from(vec![region, init, next]);
        let cmp = push(&mut g, Op::Cmp(CmpOp::Lt), IrType::Int, vec![phi, n]);
        let iff = push(&mut g, Op::If, IrType::Control, vec![region, cmp]);
        let t = push(&mut g, Op::Proj(0), IrType::Control, vec![iff]);
        let _f = push(&mut g, Op::Proj(1), IrType::Control, vec![iff]);
        g.nodes[region as usize].inputs = Inputs::from(vec![c0, t]);
        g.exit = push(&mut g, Op::Return, IrType::Void, vec![t, phi]);
        let sched = crate::ir_schedule::schedule(&g);
        let tb = sched.node_to_block[t as usize];

        let pr = analyze_graph_pathwise(&g, &sched);
        let body = pr.at(phi, tb);
        assert!(
            !body.definitely_non_negative(),
            "the loop counter starts at -5 and the body runs with it; any \
             analysis answering non-negative here would delete a bounds check \
             the first iteration needs. Got {body:?}",
        );
        match body.lo() {
            None => {} // bottom is not a claim about a value; nothing to check
            Some(lo) => assert!(
                lo <= -5,
                "the phi's lower bound must still admit its entry value, got {lo}",
            ),
        }
    }

    /// A refinement result is never ABOVE the flow-insensitive answer.
    ///
    /// The overlay is only ever allowed to narrow. An entry above the global
    /// range would be a second, weaker opinion about the same value, and `at`
    /// would then disagree with `of` in the permissive direction.
    #[test]
    fn every_narrowed_entry_is_below_the_whole_method_answer() {
        let (g, sched, _p, _tb, _fb) = sign_guarded_graph();
        let pr = analyze_graph_pathwise(&g, &sched);
        for b in 0..sched.blocks.len() {
            for id in 0..g.nodes.len() as NodeId {
                let global = pr.of(id);
                let local = pr.at(id, b);
                assert!(
                    global.contains_all(local),
                    "node {id} in block {b}: {local:?} is not inside {global:?}",
                );
            }
        }
    }

    /// A non-converged fixpoint records no facts at all: every base range is
    /// top, so there is nothing to narrow from and nothing that could be
    /// narrowed to something wrong.
    #[test]
    fn a_refused_fixpoint_records_no_facts() {
        let (g, sched, p, tb, _fb) = sign_guarded_graph();
        let pr = analyze_graph_pathwise(&g, &sched);
        // Positive control first -- this fixture DOES converge and DOES narrow,
        // so the assertion below is about the refusal and not about the shape.
        assert!(pr.converged() && !pr.at(p, tb).is_top());

        let refused = PathRanges::flow_insensitive(GraphRanges {
            ranges: vec![Range::top(W); g.nodes.len()],
            converged: false,
        });
        assert!(!refused.converged());
        for b in 0..sched.blocks.len() {
            assert_eq!(refused.at(p, b), refused.of(p));
        }
    }
}
