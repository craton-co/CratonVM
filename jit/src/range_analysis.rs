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

use crate::ir::{node_id_opt, Graph, IrType, MemKind, NodeId, Op};
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

    /// Whether every admitted value is `>= 0`. Bottom answers `true`
    /// vacuously — bottom means the value does not exist, so every claim about
    /// it holds; callers that care about reachability must test
    /// [`Range::is_empty`] themselves.
    pub fn is_non_negative(self) -> bool {
        match self.iv {
            None => true,
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

/// The range of the value at input slot `slot` of node `id`, or top.
fn input_range(g: &Graph, ranges: &[Range], id: usize, slot: usize, w: IntWidth) -> Range {
    let Some(raw) = g.nodes[id].inputs.get(slot).copied() else {
        return Range::top(w);
    };
    let Some(idx) = node_id_opt(raw) else {
        return Range::top(w);
    };
    match ranges.get(idx as usize) {
        Some(r) => *r,
        None => Range::top(w),
    }
}

/// One node's transfer function.
fn transfer(g: &Graph, ranges: &[Range], id: usize, w: IntWidth) -> Range {
    let node = &g.nodes[id];
    let a = |slot: usize| input_range(g, ranges, id, slot, w);
    // A shift count is always an `int`, even for a `long` shiftee.
    let count = |slot: usize| input_range(g, ranges, id, slot, IntWidth::W32);
    match &node.op {
        Op::Const(v) => Range::constant(w, *v),
        Op::Phi => {
            let mut acc = Range::empty(w);
            for inp in node.phi_value_inputs() {
                acc = match inp.and_then(|i| ranges.get(i as usize).copied()) {
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
        Op::I2L => input_range(g, ranges, id, 0, IntWidth::W32).i2l(),
        Op::L2I => input_range(g, ranges, id, 0, IntWidth::W64).l2i(),
        Op::I2B => input_range(g, ranges, id, 0, IntWidth::W32).i2b(),
        Op::I2C => input_range(g, ranges, id, 0, IntWidth::W32).i2c(),
        Op::I2S => input_range(g, ranges, id, 0, IntWidth::W32).i2s(),
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

    #[test]
    fn array_length_and_mem_kinds() {
        assert_eq!(Range::array_length(), r(0, i32::MAX as i64));
        assert!(Range::array_length().is_non_negative());
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
}
