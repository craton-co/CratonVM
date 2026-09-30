// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `tableswitch` / `lookupswitch` operand validation.
//!
//! Moved verbatim out of `x64.rs`'s `Switch-instruction validation helpers (HIGH security, task #8)`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

//
// Adversarial bytecode can pass `tableswitch` ranges or `lookupswitch` npair
// counts that overflow when treated naively as `usize`. The interpreter
// validates these at verify-time; the JIT must do the same before allocating
// a jump table of that size. Both helpers return `None` on any overflow OR
// cap exceeded → the caller falls back to the interpreter, no panic.

/// Maximum tableswitch entries we'll lay out as a dense jump table.
/// Matches the JVMS upper bound and is well under any reasonable code-cache
/// size budget; anything larger is almost certainly an adversarial input.
pub(super) const MAX_TABLESWITCH_COUNT: usize = 1 << 24;

/// Maximum lookupswitch pairs we'll emit. lookupswitch is sparser than
/// tableswitch so a tighter cap is appropriate.
pub(super) const MAX_LOOKUPSWITCH_NPAIRS: usize = 1 << 20;

/// Validate `high - low + 1` against signed-overflow AND a sane upper bound.
/// Returns the count as `usize` if both checks pass.
pub fn checked_tableswitch_count(low: i32, high: i32) -> Option<usize> {
    // Widening: i32 -> i64 (sign-extended, avoids overflow in high-low+1)
    let span = (high as i64).checked_sub(low as i64)?;
    let count = span.checked_add(1)?;
    if count <= 0 {
        return None;
    }
    let count = usize::try_from(count).ok()?;
    if count > MAX_TABLESWITCH_COUNT {
        return None;
    }
    Some(count)
}

/// Validate `npairs` (the count of (match, offset) pairs in a lookupswitch
/// payload) against negative values AND a sane upper bound.
pub fn checked_lookupswitch_npairs(npairs: i32) -> Option<usize> {
    if npairs < 0 {
        return None;
    }
    // Cast: non-negative index/count to usize
    let n = npairs as usize;
    if n > MAX_LOOKUPSWITCH_NPAIRS {
        return None;
    }
    Some(n)
}

// r10-ops: this file had no direct unit tests before this round — the two
// helpers are pure and adversarial-input-facing (a hostile `high`/`low` or
// `npairs` triple straight from the class file), so their boundaries are
// cheap to pin down without a compiled JIT at all.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tableswitch_single_case_is_one() {
        // `high == low`: exactly one case, JVMS allows it.
        assert_eq!(checked_tableswitch_count(0, 0), Some(1));
        assert_eq!(checked_tableswitch_count(5, 5), Some(1));
    }

    #[test]
    fn tableswitch_high_below_low_is_refused() {
        // `count = high - low + 1 <= 0` — an empty or negative span, which a
        // well-formed tableswitch never has. Must refuse, not wrap or emit a
        // zero/negative-size table.
        assert_eq!(checked_tableswitch_count(5, 3), None);
        assert_eq!(checked_tableswitch_count(5, 4), None); // count == 0
    }

    #[test]
    fn tableswitch_full_i32_span_exceeds_the_cap() {
        // The widest legal `low`/`high` pair: span is ~2^32, far past
        // `MAX_TABLESWITCH_COUNT` (2^24). Must refuse rather than allocate a
        // multi-GiB jump table for adversarial bytecode.
        assert_eq!(checked_tableswitch_count(i32::MIN, i32::MAX), None);
    }

    #[test]
    fn tableswitch_cap_boundary() {
        let low = 0;
        let at_cap = i32::try_from(MAX_TABLESWITCH_COUNT - 1).expect("cap fits i32");
        let over_cap = i32::try_from(MAX_TABLESWITCH_COUNT).expect("cap fits i32");
        assert_eq!(
            checked_tableswitch_count(low, at_cap),
            Some(MAX_TABLESWITCH_COUNT)
        );
        assert_eq!(checked_tableswitch_count(low, over_cap), None);
    }

    #[test]
    fn lookupswitch_negative_npairs_is_refused() {
        assert_eq!(checked_lookupswitch_npairs(-1), None);
        assert_eq!(checked_lookupswitch_npairs(i32::MIN), None);
    }

    #[test]
    fn lookupswitch_zero_npairs_is_a_default_only_switch() {
        assert_eq!(checked_lookupswitch_npairs(0), Some(0));
    }

    #[test]
    fn lookupswitch_cap_boundary() {
        let at_cap = i32::try_from(MAX_LOOKUPSWITCH_NPAIRS).expect("cap fits i32");
        let over_cap = at_cap + 1;
        assert_eq!(
            checked_lookupswitch_npairs(at_cap),
            Some(MAX_LOOKUPSWITCH_NPAIRS)
        );
        assert_eq!(checked_lookupswitch_npairs(over_cap), None);
    }

    #[test]
    fn lookupswitch_i32_max_exceeds_the_cap() {
        assert_eq!(checked_lookupswitch_npairs(i32::MAX), None);
    }
}
