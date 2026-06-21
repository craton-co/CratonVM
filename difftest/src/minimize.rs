// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Reproducer minimizer (design §3.4).
//!
//! On a confirmed divergence, shrink to a minimal reproducer that **still
//! diverges** (re-confirmed each step) under the interestingness predicate
//! "still compiles AND still diverges":
//! - **source tier:** ddmin line-/statement-/token-level deletion on the
//!   generated `.java`;
//! - **bytecode tier:** `cargo fuzz tmin` with the predicate swapped from
//!   "panics" to "diverges from HotSpot".
//!
//! ## Status: Step 0
//!
//! Stub only — [`minimize_source`] returns the input unchanged. The ddmin
//! shrinker lands in Step 6, writing the minimized artifact under
//! `difftest/regression/<id>/`.

/// Outcome of a minimization pass.
#[derive(Debug, Clone)]
pub struct Minimized {
    /// The (possibly unchanged) reduced source.
    pub source: String,
    /// Number of shrink steps that kept the program divergent.
    pub steps: usize,
}

/// Shrink `source` while `still_diverges` holds.
///
/// **Step 0 stub:** returns `source` unchanged with zero steps and never calls
/// the predicate. Step 6 implements ddmin: delete a span, re-`javac`, re-run
/// both VMs via the predicate, keep the deletion iff it still diverges.
pub fn minimize_source<F>(source: &str, _still_diverges: F) -> Minimized
where
    F: Fn(&str) -> bool,
{
    Minimized {
        source: source.to_string(),
        steps: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimize_is_identity_in_step0() {
        let m = minimize_source("class A {}", |_| true);
        assert_eq!(m.source, "class A {}");
        assert_eq!(m.steps, 0);
    }
}
