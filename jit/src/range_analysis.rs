// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! `jit/src/scev.rs` already carries an interval lattice
//! ([`IntRange`](crate::scev::IntRange)) and a
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
//! | consumer | counted-loop bounds proofs | SSA node ranges for `ir_check_elim` (the optimizing tier's `idx >= 0` half); `x64/bce.rs` uses only the `Range` type, in its default-off `range_safe_pcs` |
//!
//! They agree on the important thing — top proves nothing, and a wrapping
//! computation is never silently treated as exact. `Range::to_scev` /
//! `Range::from_scev` convert between them. They are test-only (round 11 wave
//! 3): no proof hands facts across today.
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
        self.meet(Range::exact(self.width, self.width.min(), bh))    }

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
        self.meet(Range::exact(self.width, bl, self.width.max()))    }

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

    // ---- JIT round 22 (lane loops; `jit-r10-loop-proposals.md` item 3) ----
    //
    // The sign-aware forms of `or` / `xor` / `ushr`, read by the node fixpoint
    // under `CRATONVM_JIT_IR_RANGE_SIGNED_BITWISE` (opt-in,
    // [`signed_bitwise_ranges_enabled`]). Each agrees with its plain twin when
    // both operands are non-negative (it calls it) and is exact-then-narrow
    // like every other transfer here; each only ever answers a SUBSET of what
    // the plain twin answers.

    /// `ior` where an operand may be negative. A negative result needs a
    /// negative operand `x`, and `x | y` only sets bits of `x`, so (among
    /// negative values the two's-complement order is the unsigned one) it is
    /// at least `x`, hence at least `min(al, bl)`. A non-negative result needs
    /// both operands non-negative and is then at most `fill_ones(max(ah, bh))`;
    /// an operand that is entirely negative leaves no such result (`-1`).
    pub fn or_signed(self, other: Range) -> Range {
        let ((al, ah), (bl, bh)) = match self.binary_operands(other) {
            Err(short) => return short,
            Ok(v) => v,
        };
        if al >= 0 && bl >= 0 {
            return self.or(other);
        }
        let hi = if ah < 0 || bh < 0 {
            -1
        } else {
            fill_ones(ah.max(bh))
        };
        Range::narrow(self.width, al.min(bl), hi)
    }

    /// `ixor` where an operand may be negative. `~v = -v - 1` maps a negative
    /// value to a non-negative one, and `x ^ y` is `(~x) ^ (~y)` (both
    /// negative), `~((~x) ^ y)` (only `x` negative) or `x ^ y` (neither), so
    /// with `m` the largest of the operands' non-negative values and
    /// complemented negative values the result is in `[0, fill_ones(m)]` when
    /// the signs agree, in `[-fill_ones(m) - 1, -1]` when they differ, and in
    /// the hull of the two when an operand straddles zero.
    pub fn xor_signed(self, other: Range) -> Range {
        let ((al, ah), (bl, bh)) = match self.binary_operands(other) {
            Err(short) => return short,
            Ok(v) => v,
        };
        if al >= 0 && bl >= 0 {
            return self.xor(other);
        }
        let magnitude = |l: i128, h: i128| -> i128 {
            let neg = if l < 0 { -l - 1 } else { 0 };
            let pos = if h >= 0 { h } else { 0 };
            neg.max(pos)
        };
        let f = fill_ones(magnitude(al, ah).max(magnitude(bl, bh)));
        let (a_neg, a_pos) = (ah < 0, al >= 0);
        let (b_neg, b_pos) = (bh < 0, bl >= 0);
        let (lo, hi) = if (a_neg && b_neg) || (a_pos && b_pos) {
            (0, f)
        } else if (a_neg && b_pos) || (a_pos && b_neg) {
            (-f - 1, -1)
        } else {
            (-f - 1, f)
        };
        Range::narrow(self.width, lo, hi)
    }

    /// `iushr` / `lushr` with the entirely-negative input made exact: the
    /// unsigned pattern of a negative `v` is `v + 2^bits`, monotone in `v`, so
    /// a constant shift `s > 0` maps `[lo, hi]` (`hi < 0`) onto
    /// `[(lo + 2^bits) >> s, (hi + 2^bits) >> s]`. A range straddling zero
    /// keeps [`Range::ushr`]'s answer (the hull of the two images is it).
    pub fn ushr_signed(self, count: Range) -> Range {
        let (lo, hi) = match self.wide() {
            None => return self,
            Some(v) => v,
        };
        let Some(c) = count.as_constant() else {
            return Range::top(self.width);
        };
        // Cast: JVMS shift-count masking, as in `shl`.
        let s = (c as u64 as u32) & self.width.shift_mask();
        if s == 0 || lo >= 0 || hi >= 0 {
            return self.ushr(count);
        }
        let m = 1i128 << self.width.bits();
        Range::narrow(self.width, (lo + m) >> s, (hi + m) >> s)
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
            | Op::StrCharDecode
    ) || matches!(op, Op::ScalarIntrinsic(_) if crate::ir::ir_scalar_range_fold_enabled())
}

/// JIT round 19 wave 2 (lane irops2, IROPS19-3): the interval transfer of an
/// integer-valued [`crate::ir::ScalarOp`], under
/// `CRATONVM_JIT_IR_SCALAR_RANGE_FOLD`. `a(k)` is input `k`'s range at the
/// result width `w` (every family read here has operands of its result's
/// width; the `int`-valued scans and counts of a `long` read no operand).
///
/// Each arm is exact arithmetic over the endpoints, total, and answers bottom
/// for a bottom operand like every other transfer:
///
/// * `min`/`max` -- monotone in both operands, so the endpoint-wise min/max;
/// * `abs` -- `[l, h]` itself when `l >= 0`, `[-h, -l]` when `h <= 0`,
///   `[0, max(-l, h)]` across zero, and TOP when `l` is the width's minimum
///   (`Math.abs(MIN_VALUE)` is `MIN_VALUE`, JLS 15.15.4);
/// * `compare` -- `{-1, 0, 1}`;
/// * leading / trailing zero counts and `bitCount` -- `[0, 32]` / `[0, 64]`;
/// * `highestOneBit` / `lowestOneBit` of a non-negative operand -- `[0, h]`
///   (both answers are a single bit of the operand, or zero), top otherwise;
/// * everything else (byte reversal, rotates, the 128-bit products, the raw
///   FP bit moves) -- top.
fn scalar_intrinsic_range(
    sop: crate::ir::ScalarOp,
    a: &dyn Fn(usize) -> Range,
    w: IntWidth,
) -> Range {
    use crate::ir::ScalarOp as S;
    match sop {
        S::MinI | S::MinL | S::MaxI | S::MaxL => {
            let (x, y) = (a(0), a(1));
            match (x.lo(), x.hi(), y.lo(), y.hi()) {
                (Some(xl), Some(xh), Some(yl), Some(yh)) => {
                    if matches!(sop, S::MinI | S::MinL) {
                        Range::exact(w, xl.min(yl), xh.min(yh))
                    } else {
                        Range::exact(w, xl.max(yl), xh.max(yh))
                    }
                }
                _ => Range::empty(w),
            }
        }
        S::AbsI | S::AbsL => {
            let x = a(0);
            match (x.lo(), x.hi()) {
                (Some(l), Some(h)) => {
                    if l == w.min() {
                        Range::top(w)
                    } else if l >= 0 {
                        Range::exact(w, l, h)
                    } else if h <= 0 {
                        Range::exact(w, -h, -l)
                    } else {
                        Range::exact(w, 0, (-l).max(h))
                    }
                }
                _ => Range::empty(w),
            }
        }
        S::CompareI | S::CompareL => Range::exact(w, -1, 1),
        S::NlzI | S::NtzI | S::BitCountI => Range::exact(w, 0, 32),
        S::NlzL | S::NtzL | S::BitCountL => Range::exact(w, 0, 64),
        // JIT round 20 wave 11 (lane pIR): the code-point tests.
        S::CharCount => Range::exact(w, 1, 2),
        S::IsBmpCodePoint | S::IsValidCodePoint | S::IsIsoControl => Range::exact(w, 0, 1),
        S::HighestOneBitI | S::HighestOneBitL | S::LowestOneBitI | S::LowestOneBitL => {
            let x = a(0);
            match (x.lo(), x.hi()) {
                (Some(l), Some(h)) if l >= 0 => Range::exact(w, 0, h),
                (Some(_), Some(_)) => Range::top(w),
                _ => Range::empty(w),
            }
        }
        _ => Range::top(w),
    }
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

    let order = sweep_order(g);
    let mut converged = false;
    for round in 0..MAX_GRAPH_ROUNDS {
        let mut changed = false;
        for &id in &order {
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

/// The order [`analyze_graph`] sweeps its modelled nodes in: every node after
/// the modelled inputs it reads, with a φ's value inputs (the only edges a
/// cycle can run through) not counted as dependences.
///
/// A depth-first post-order rooted at each modelled node in ascending id, so
/// when id order is already such an order (the builder's output) this IS id
/// order and the sweep is exactly what it was. After a pass rewires a use to a
/// later-numbered definition, an id-order sweep needed one more round per such
/// edge on the chain, and each round spent brought the φs closer to
/// [`WIDEN_AFTER_ROUND`] and the whole fixpoint closer to the
/// [`MAX_GRAPH_ROUNDS`] give-up, which discards every fact (round 11 wave 3,
/// `r11-iropt-check-elim-per-block-fact-clones` item 4).
///
/// Soundness does not rest on the order: the sweep only stops on a round that
/// changed nothing, so whatever order it ran in, the result is a post-fixpoint
/// of the transfer functions. A non-φ cycle (malformed) is cut at the edge that
/// closes it; that only costs a round.
fn sweep_order(g: &Graph) -> Vec<usize> {
    let n = g.nodes.len();
    let modelled = |id: usize| -> bool {
        g.nodes
            .get(id)
            .is_some_and(|node| width_of(node.ty).is_some() && is_modelled(&node.op))
    };
    // 0 = unvisited, 1 = on the DFS stack, 2 = emitted.
    let mut state = vec![0u8; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if state[root] != 0 || !modelled(root) {
            continue;
        }
        state[root] = 1;
        stack.push((root, 0));
        while let Some(top) = stack.last_mut() {
            let (id, edge) = *top;
            let deps: &[NodeId] = match g.nodes.get(id) {
                Some(node) if node.op != Op::Phi => node.inputs.as_slice(),
                _ => &[],
            };
            match deps.get(edge) {
                Some(&inp) => {
                    top.1 = edge + 1;
                    let i = inp as usize;
                    if i < n && state[i] == 0 && modelled(i) {
                        state[i] = 1;
                        stack.push((i, 0));
                    }
                }
                None => {
                    state[id] = 2;
                    order.push(id);
                    stack.pop();
                }
            }
        }
    }
    order
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
    /// Every fact, stored ONCE, against the dominator subtree it holds in.
    ///
    /// Round 11 wave 2 stopped cloning a dominator's map into every block
    /// below it, but a block that added a fact still copied everything it
    /// inherited, so a chain of N guards on different values cost O(N²) time
    /// and memory. Since wave 5 a block stores only what it adds, and a query
    /// finds the innermost dominator's entry (see [`FactStore`]).
    facts: FactStore,
    /// Per block, its dominator-tree preorder number; [`NO_ENTRY`] for a
    /// block the walk did not reach (no fact applies there).
    pre: Vec<u32>,
    census: PathCensus,
}

/// "None" for a preorder number and for an entry's parent link.
const NO_ENTRY: u32 = u32::MAX;

/// One path fact: `range` holds for a use in every block whose dominator
/// preorder number is in `lo..=hi` (the dominator subtree of the block that
/// established it), unless a deeper entry for the same value says otherwise.
#[derive(Clone, Copy)]
struct FactEntry {
    lo: u32,
    hi: u32,
    range: Range,
    /// Index (same list) of the innermost entry whose interval strictly
    /// contains this one, or [`NO_ENTRY`]. Always a smaller index.
    parent: u32,
}

/// Path facts per value, as lists of dominator-subtree intervals.
///
/// Dominator subtrees are contiguous preorder intervals and either nest or
/// are disjoint, and the walk adds entries in preorder, so each list is
/// sorted by `lo` and forms a forest under `parent`. The fact for a use at
/// preorder `p` is the innermost interval containing `p`: the last entry with
/// `lo <= p`, or, when that one ended before `p` (a sibling subtree), the
/// first of its ancestors that reaches `p` — any interval containing `p`
/// starts before that entry, so it contains it too.
#[derive(Default)]
struct FactStore {
    by_value: rustc_hash::FxHashMap<NodeId, Vec<FactEntry>>,
}

impl FactStore {
    /// Index of the innermost entry of `list` whose interval contains `p`.
    fn innermost(list: &[FactEntry], p: u32) -> Option<usize> {
        let mut k = list.partition_point(|e| e.lo <= p).checked_sub(1)?;
        loop {
            let e = list.get(k)?;
            if e.hi >= p {
                return Some(k);
            }
            // `parent < k` by construction; checked so a corrupt link ends
            // the walk instead of looping.
            if e.parent == NO_ENTRY || e.parent as usize >= k {
                return None;
            }
            k = e.parent as usize;
        }
    }

    /// The fact for `id` at dominator preorder `p`, if any dominator set one.
    fn lookup(&self, id: NodeId, p: u32) -> Option<Range> {
        let list = self.by_value.get(&id)?;
        Self::innermost(list, p)
            .and_then(|k| list.get(k))
            .map(|e| e.range)
    }

    /// Record `range` for `id` over `lo..=hi`. Callers add in preorder, so
    /// `lo` is larger than every `lo` already in the list.
    fn insert(&mut self, id: NodeId, lo: u32, hi: u32, range: Range) {
        let list = self.by_value.entry(id).or_default();
        let parent = Self::innermost(list, lo).map_or(NO_ENTRY, |k| k as u32);
        list.push(FactEntry {
            lo,
            hi,
            range,
            parent,
        });
    }
}

/// Block `b`'s view while its own facts are computed: what it adds, over
/// what its dominators established, over the whole-method answer.
struct BlockView<'a> {
    base: &'a GraphRanges,
    store: &'a FactStore,
    pre: u32,
    own: rustc_hash::FxHashMap<NodeId, Range>,
}

impl BlockView<'_> {
    fn get(&self, id: NodeId) -> Range {
        if let Some(r) = self.own.get(&id) {
            return *r;
        }
        self.store
            .lookup(id, self.pre)
            .unwrap_or_else(|| self.base.of(id))
    }

    fn insert(&mut self, id: NodeId, r: Range) {
        self.own.insert(id, r);
    }
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
            facts: FactStore::default(),
            pre: Vec::new(),
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
        match self.pre.get(block) {
            Some(&p) if p != NO_ENTRY => {
                self.facts.lookup(id, p).unwrap_or_else(|| self.base.of(id))
            }
            _ => self.base.of(id),
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
    let mut census = PathCensus::default();

    if !base.converged() {
        return PathRanges {
            base,
            facts: FactStore::default(),
            pre: Vec::new(),
            census,
        };
    }

    // One user index for the whole graph. `Graph::users_of` falls back to a
    // full scan when the use lists are not current, which inside a per-node
    // loop would be quadratic; this is O(nodes + edges), once.
    let users = build_user_index(g);

    let order = dom_preorder(schedule);
    let (pre, last) = preorder_intervals(schedule, &order);
    let mut store = FactStore::default();

    for &b in &order {
        let Some(&p) = pre.get(b) else {
            continue;
        };
        let Some((cond, taken)) = entry_edge_condition(g, schedule, b) else {
            continue;
        };

        // Everything a dominator established is read through `store`: the
        // walk visits a block only after its whole dominator chain, and
        // nothing visited earlier that does not dominate `b` has an interval
        // containing `p`.
        let mut view = BlockView {
            base: &base,
            store: &store,
            pre: p,
            own: rustc_hash::FxHashMap::default(),
        };
        let pinned = apply_condition(g, &mut view, cond, taken);
        if pinned.is_empty() {
            // `narrow_pair` inserts only what it pins, so the block adds
            // nothing to what it inherits.
            census.unmodelled_conditions += 1;
            continue;
        }
        census.blocks_with_facts += 1;
        census.pinned += pinned.len();
        refine(g, &users, &mut view, &pinned, &mut census);
        let own = view.own;
        let hi = last.get(b).copied().unwrap_or(p);
        for (id, range) in own {
            store.insert(id, p, hi, range);
        }
    }

    PathRanges {
        base,
        facts: store,
        pre,
        census,
    }
}

/// Per block, its position in `order` (a dominator-tree preorder from
/// [`dom_preorder`]) and the last position of its dominator subtree, which is
/// contiguous in a preorder. [`NO_ENTRY`] for a block `order` omits.
fn preorder_intervals(schedule: &Schedule, order: &[usize]) -> (Vec<u32>, Vec<u32>) {
    let n = schedule.blocks.len();
    let mut pre = vec![NO_ENTRY; n];
    for (i, &b) in order.iter().enumerate() {
        if let Some(slot) = pre.get_mut(b) {
            *slot = i as u32;
        }
    }
    let mut last = pre.clone();
    // Reverse preorder finishes every descendant before its ancestor. The
    // edge test is `dom_preorder`'s own (`p != b && p < n`) plus `pp < pb`,
    // which every tree edge of a preorder satisfies.
    for &b in order.iter().rev() {
        let Some(p) = schedule.dom.idom(b) else {
            continue;
        };
        if p == b {
            continue;
        }
        let (Some(&pb), Some(&pp), Some(&lb)) = (pre.get(b), pre.get(p), last.get(b)) else {
            continue;
        };
        if pp < pb {
            if let Some(lp) = last.get_mut(p) {
                *lp = (*lp).max(lb);
            }
        }
    }
    (pre, last)
}

/// `index[i]` = the distinct nodes naming `i` as an input.
fn build_user_index(g: &Graph) -> Vec<Vec<NodeId>> {
    let mut index: Vec<Vec<NodeId>> = vec![Vec::new(); g.nodes.len()];
    for (uid, node) in g.nodes.iter().enumerate() {
        for &raw in node.inputs.as_slice() {
            if let Some(i) = node_id_opt(raw) {
                if let Some(slot) = index.get_mut(i as usize) {
                    // `uid` only grows, so a duplicate can only be the entry
                    // just pushed. `contains` made a hub node (an interned
                    // `Const`, the receiver) quadratic in its user count.
                    if slot.last() != Some(&(uid as NodeId)) {
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
fn apply_condition(g: &Graph, facts: &mut BlockView<'_>, cond: NodeId, taken: bool) -> Vec<NodeId> {
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
        return narrow_pair(g, facts, a, b, op);
    }

    narrow_pair(g, facts, l, r, op)
}

/// Narrow `l` and `r` by `l op r`, returning whichever actually moved.
fn narrow_pair(
    g: &Graph,
    facts: &mut BlockView<'_>,
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

    let cl = facts.get(l);
    let cr = facts.get(r);
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
    users: &[Vec<NodeId>],
    facts: &mut BlockView<'_>,
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
                        Some(facts.get(n))
                    };
                    transfer_via(g, u as usize, w, &view)
                };

                let curr = facts.get(u);
                // Clamp to what is already known. The refinement may only ever
                // narrow: an overlay entry ABOVE the current answer would be a
                // second, weaker opinion about the same value, and `at` would
                // then disagree with `of` in the unsafe direction.
                // `curr` first: `Range::meet` returns its RECEIVER when the two
                // widths disagree, and the receiver must be the answer already
                // held, not the recomputed one — `computed.meet(curr)` let a
                // width-mismatched recomputation replace, and widen, the fact
                // (round 11).
                let nextr = curr.meet(computed);
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

/// `CRATONVM_JIT_IR_RANGE_SIGNED_BITWISE` -- **default OFF** (JIT round 22
/// wave 1, lane loops, `jit-r10-loop-proposals.md` item 3; perf, opt-in): the
/// node fixpoint reads [`Range::or_signed`], [`Range::xor_signed`] and
/// [`Range::ushr_signed`] instead of the plain forms, which answer top (or a
/// width-wide bound) as soon as an operand may be negative. Read per `Or` /
/// `Xor` / `UShr` node transfer only (no `static`), as
/// `ir::ir_scalar_range_fold_enabled` is for a scalar intrinsic.
fn signed_bitwise_ranges_enabled() -> bool {
    false}

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
        // JIT round 22 (lane loops), opt-in: the sign-aware forms.
        Op::Or if signed_bitwise_ranges_enabled() => a(0).or_signed(a(1)),
        Op::Or => a(0).or(a(1)),
        Op::Xor if signed_bitwise_ranges_enabled() => a(0).xor_signed(a(1)),
        Op::Xor => a(0).xor(a(1)),
        Op::Shl => a(0).shl(count(1)),
        Op::Shr => a(0).shr(count(1)),
        Op::UShr if signed_bitwise_ranges_enabled() => a(0).ushr_signed(count(1)),
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
        // STRINTR21-5: a zero-extended UTF16 unit or LATIN1 byte, a `char`.
        Op::StrCharDecode => Range::exact(IntWidth::W32, 0, 65535),
        // JIT round 19 wave 2 (lane irops2, IROPS19-3), opt-in.
        Op::ScalarIntrinsic(sop) if crate::ir::ir_scalar_range_fold_enabled() => {
            scalar_intrinsic_range(*sop, &a, w)
        }
        _ => Range::top(w),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;

// ---- interop with `scev::IntRange` (test-only) ------------------------------
//
// No production caller hands a fact between the two lattices yet (round 11
// wave 3, `r11-iropt-dead-and-test-only-api-in-the-mid-end`). Kept at the end
// of the file, after every production line the panic-free-compile ratchet
// scans, for the tests that pin the conversion.
#[cfg(test)]
impl Range {
    /// This range as a `scev::IntRange`, for handing a fact to the
    /// counted-loop proof engine. `None` for a 64-bit range — `IntRange` is an
    /// `int` lattice and silently truncating into it would be exactly the
    /// class of bug this module exists to prevent.
    pub fn to_scev(self) -> Option<crate::scev::IntRange> {
        if self.width != IntWidth::W32 {
            return None;
        }
        Some(match self.iv {
            None => crate::scev::IntRange::empty(),
            // Cast: both endpoints are inside W32 by the `exact` invariant.
            Some((lo, hi)) => crate::scev::IntRange::new(lo as i32, hi as i32),
        })
    }

    /// A `scev::IntRange` lifted into this lattice at [`IntWidth::W32`].
    pub fn from_scev(r: crate::scev::IntRange) -> Range {
        match (r.lo(), r.hi()) {
            // Widening: i32 endpoints into the i64 interval representation.
            (Some(lo), Some(hi)) => Range::exact(IntWidth::W32, lo as i64, hi as i64),
            _ => Range::empty(IntWidth::W32),
        }
    }
}

// JIT round 22 wave 1 (lane loops): the sign-aware bitwise transfers, checked
// against every value pair of small `int` ranges (and the corners).
#[cfg(test)]
mod r22_loops_signed_bitwise_tests;
