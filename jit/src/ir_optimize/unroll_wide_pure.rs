// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 8 (lane W8LOOP; the "wider unroll for a register-only body" lever
//! of `docs/known-issues/jit/jr21-promoted-loop-bodies-spill-and-poll-every-trip-20261026.md`):
//! the main/post unroll's factor for a body none of whose nodes can deopt,
//! `CRATONVM_JIT_IR_UNROLL_WIDE_PURE=1` (opt-in).
//!
//! HotSpot's C2 unrolls a small register-only counted loop 8-16x
//! (`LoopUnrollLimit`), so `counterInt` runs ~1.4 instructions per iteration;
//! CratonVM's main loop is 4 copies (`partial_unroll_factor`), so the latch
//! (test, poll, jump) is paid once per 4 trips. A body that cannot deopt needs
//! no per-copy frames (`main_post_unroll_loop`'s `frames == None` route), so
//! widening it adds only code: the copies, and on a promoted accumulation the
//! reassociation (`linear_reassoc`) folds them back to one add per cell.
//!
//! The rule ([`main_post_factor`]): with the switch on and no explicit
//! `CRATONVM_JIT_IR_PARTIAL_UNROLL_FACTOR`, the largest factor up to
//! [`WIDE_PURE_MAX_FACTOR`] whose total stays inside the unroller's body
//! budget, never below the base factor. Everything else -- the stay test,
//! the licence `n - d < n`, the post loop -- is `main_post_unroll_loop`'s,
//! which already admits any factor in `2..=8` (`d = (factor - 1) * s` fits an
//! `int` for `s <= 64`).
//!
//! Opt-in because it was written without a build: code growth against the
//! acceptance gate is exactly what the A/B must price.

/// The widest factor this lever asks for: `main_post_unroll_loop`'s own cap.
pub(super) const WIDE_PURE_MAX_FACTOR: usize = 8;

/// `CRATONVM_JIT_IR_UNROLL_WIDE_PURE=1` (perf, **opt-in**; j27 wave 8, lane
/// W8LOOP). Read per loop offered; no `static`.
fn unroll_wide_pure_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_UNROLL_WIDE_PURE")
}

/// Did the run name the partial unroll factor itself? Then it decides.
fn factor_pinned_by_user() -> bool {
    cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_PARTIAL_UNROLL_FACTOR").is_ok()
}

/// The pure rule: the factor a `body_len`-node body gets under `budget`.
/// `base` when the body can deopt, when the switch is off, or when nothing
/// wider fits.
pub(super) fn wide_factor(base: usize, body_len: usize, pure: bool, budget: usize) -> usize {
    if !pure || body_len == 0 {
        return base;
    }
    let fits = budget / body_len;
    fits.min(WIDE_PURE_MAX_FACTOR).max(base)
}

/// The factor `main_post_unroll_loop` is offered for this loop. `pure`: no
/// node of the body can deopt (the `frames == None` route).
pub(super) fn main_post_factor(base: usize, body_len: usize, pure: bool, budget: usize) -> usize {
    if !unroll_wide_pure_enabled() || factor_pinned_by_user() {
        return base;
    }
    wide_factor(base, body_len, pure, budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn j27_w8_wide_pure_takes_the_widest_factor_the_budget_holds() {
        // A 5-node promoted accumulation under the 48-node budget: 8 copies.
        assert_eq!(wide_factor(4, 5, true, 48), 8);
        // 10 nodes: 48 / 10 = 4 copies, the base.
        assert_eq!(wide_factor(4, 10, true, 48), 4);
        // 7 nodes: 6 copies (42 <= 48).
        assert_eq!(wide_factor(4, 7, true, 48), 6);
    }

    #[test]
    fn j27_w8_wide_pure_never_narrows_and_leaves_a_deopting_body_alone() {
        assert_eq!(wide_factor(4, 5, false, 48), 4, "a body that can deopt keeps the base");
        assert_eq!(wide_factor(4, 20, true, 48), 4, "never below the base");
        assert_eq!(wide_factor(4, 0, true, 48), 4, "an empty body is refused elsewhere");
    }

    #[test]
    fn j27_w8_wide_pure_is_opt_in_and_yields_to_an_explicit_factor() {
        let run = |wide: Option<&str>, pinned: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT_IR_UNROLL_WIDE_PURE", wide),
                    ("CRATONVM_JIT_IR_PARTIAL_UNROLL_FACTOR", pinned),
                ],
                || main_post_factor(4, 5, true, 48),
            )
        };
        assert_eq!(run(None, None), 4, "unset is off");
        assert_eq!(run(Some("1"), None), 8, "=1 widens a pure body");
        assert_eq!(run(Some("1"), Some("4")), 4, "an explicit factor decides");
    }
}
