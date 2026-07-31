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
