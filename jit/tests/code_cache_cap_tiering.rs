// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The tier manager's half of the code-cache cap: a cap refusal is DEFERRED,
//! never charged, and released when the cap clears.
//!
//! # The defect (RT-8 soak, 2026-09-18)
//!
//! `compile_gate::CompileRefusal::CodeCacheAtCapacity` is documented, and
//! tested in `code_cache_cap.rs`, as transient: one byte of headroom admits
//! again. The tier manager never asked the cap itself. It queued the request,
//! the worker's `compile_gate::admit` refused it, and the unpublished compile
//! came back to `CompilerCore::finish` looking exactly like broken codegen. So
//! it was charged to `tier_fail_count`, and after three such refusals the
//! method was settled for the life of the process. The transient refusal had
//! become permanent one layer up.
//!
//! Measured on a phased workload at a 1 MiB cap with the sweeper on: 7,062 cap
//! refusals charged as failures, and 2,328 methods that reached the
//! three-failure limit. No room a sweep freed could ever be used by them.
//!
//! # ONE `#[test]`, for the reason `code_cache_cap.rs` gives
//!
//! `COMMITTED_JIT_CODE_BYTES` is process-global and cargo runs a binary's
//! tests as threads. This file holds one test and no codegen.

use cratonvm_jit::tiered::{CompilationTier, MethodKey, TieredCompilationManager};
use cratonvm_jit::COMMITTED_JIT_CODE_BYTES;
use std::sync::atomic::Ordering;

#[test]
fn a_cap_refusal_is_deferred_uncharged_and_released_when_the_cap_clears() {
    let cap = cratonvm_jit::jit_code_cache_cap_bytes();
    if cap == usize::MAX {
        eprintln!("CRATONVM_JIT_CODE_CACHE_MAX_MB=0 disables the cap; nothing to assert");
        return;
    }
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::with_class_id(
        cratonvm_types::ClassId::new(0x00CA_CE02),
        "cratonvm/test/CapTiering",
        "hot",
        "()V",
    );
    // Hot enough to compile on the first offer, so the only thing that can stop
    // the request is the cap.
    let hot = 1_000_000;

    let raise = cap.saturating_sub(COMMITTED_JIT_CODE_BYTES.load(Ordering::Relaxed));
    COMMITTED_JIT_CODE_BYTES.fetch_add(raise, Ordering::Relaxed);
    mgr.note_code_cache_pressure(true);

    // Refused at the cap, as many times as the old retry budget and more.
    let mut stamp = 0;
    for _ in 0..5 {
        let verdict = mgr.on_method_invocation_settling(&key, hot);
        assert_eq!(
            verdict.recommended, None,
            "at the cap nothing may be queued -- the worker would only be \
             refused, and that refusal is what used to be charged"
        );
        assert_ne!(
            verdict.settled_generation, 0,
            "a deferred method is settled, so the interpreter stops asking \
             while the cap holds"
        );
        stamp = verdict.settled_generation;
    }
    assert_eq!(
        mgr.cap_refusals(),
        5,
        "each refusal is counted as a cap refusal"
    );
    assert_eq!(mgr.ineligible_refusals(), 0, "and none as ineligibility");

    // The cap clears. The drain reports it, the stamps expire, and the method
    // asks again -- and is admitted. Under the old accounting the three charged
    // failures would have refused it here for good.
    COMMITTED_JIT_CODE_BYTES.fetch_sub(raise, Ordering::Relaxed);
    assert!(
        mgr.note_code_cache_pressure(false),
        "the falling edge is reported"
    );
    assert!(
        !mgr.tiering_settled(stamp),
        "the stamp handed out at the cap must have expired"
    );
    // Which tier is the policy's business (at this count it is C2 directly);
    // that a compile is queued at all is this test's.
    let recommended = mgr.on_method_invocation_settling(&key, hot).recommended;
    assert!(
        matches!(recommended, Some(CompilationTier::C1 | CompilationTier::C2)),
        "a method refused only by the cap must compile once the cap clears; got {recommended:?}"
    );
}
