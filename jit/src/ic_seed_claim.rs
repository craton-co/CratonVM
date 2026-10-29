// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
//! j27 lane HANGHUNT (`docs/internal/fixed-bugs/jit/j27-seglongloops-heap-sum-bistable-slow-state-FIXED-20261028.md`,
//! candidate 2): when the inline-cache profile seed (`IcProfileSeedSite`, the
//! VM's `note_ic_profile_seed`) asks for its one recompile, and when that ask
//! is spent.
//!
//! The seed is armed only on a site whose receiver row was starved when the
//! body was compiled (`ir_ic_profile_seed_planned`). Before this switch the
//! VM asked the recompile only when the seed itself raised the row
//! (`ProfileStore::seed_ic_observed_receiver` answered `true`). A row that the
//! still-interpreted activations had filled meanwhile answered `false`, and
//! nothing asked: the starved body kept its generic inline-cache call for
//! good, although the profile now plans the site. Which runs lost depended on
//! whether the interpreter recorded first: a bistable outcome. The claim was
//! also spent before the C2 retry answered, so a refused retry left the body
//! starved too.

/// `CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE` (j27 lane HANGHUNT; pathology
/// fix, **default ON**, `=0` kills; the host trace of the `R26SegLongLoops`
/// `heap-sum` hang named this route, about 600x slower in 4 of 6 runs):
/// the seed asks its recompile on a class new to the site even when the row
/// was already full ([`ic_seed_asks_recompile`]), and a claim whose retry was
/// refused is handed back ([`ic_seed_claim_kept`]). Read per seed event
/// (cold: once per new class at an armed site); no static.
pub fn ic_seed_claim_on_enqueue_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE")
}

/// Should a seed event ask for the recompile? `seeded`: the seed raised the
/// row. With the switch on, a row the interpreter filled first asks too: the
/// armed seed itself proves the body was compiled starved.
pub fn ic_seed_asks_recompile(seeded: bool, claim_on_enqueue: bool) -> bool {
    seeded || claim_on_enqueue
}

/// Is the claim kept after the C2 retry answered? Always with the switch off
/// (the old rule). With it on, only when a compile will run or is owed
/// (`Enqueued` / `AlreadyQueued`); a `Refused` retry hands the claim back.
pub fn ic_seed_claim_kept(answer: crate::tiered::C2RetryAnswer, claim_on_enqueue: bool) -> bool {
    !claim_on_enqueue || answer != crate::tiered::C2RetryAnswer::Refused
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiered::C2RetryAnswer;

    const SW: &str = "CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE";

    #[test]
    fn the_switch_is_default_on_with_a_kill() {
        let on = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(&[(SW, v)], ic_seed_claim_on_enqueue_enabled)
        };
        assert!(on(None), "default ON");
        assert!(on(Some("1")));
        assert!(!on(Some("0")), "`=0` kills");
    }

    #[test]
    fn a_row_the_interpreter_filled_first_still_asks_with_the_switch() {
        assert!(ic_seed_asks_recompile(true, false), "old rule: the seed raised the row");
        assert!(!ic_seed_asks_recompile(false, false), "old rule: a filled row never asks");
        assert!(ic_seed_asks_recompile(false, true), "switch: a filled row asks");
    }

    #[test]
    fn a_refused_retry_hands_the_claim_back_only_with_the_switch() {
        assert!(ic_seed_claim_kept(C2RetryAnswer::Refused, false), "old rule");
        assert!(!ic_seed_claim_kept(C2RetryAnswer::Refused, true));
        assert!(ic_seed_claim_kept(C2RetryAnswer::Enqueued, true));
        assert!(ic_seed_claim_kept(C2RetryAnswer::AlreadyQueued, true), "owed: a compile will run");
    }
}
