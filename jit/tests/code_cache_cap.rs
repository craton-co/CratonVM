// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The code-cache cap's degradation path, which had no test at all.
//!
//! # What this covers, and why it was worth writing
//!
//! `CRATONVM_JIT_CODE_CACHE_MAX_MB` and the machinery behind it —
//! `jit_code_cache_cap_bytes`, `jit_code_cache_at_capacity`, the
//! retirement-queue drain-and-retry, `CompileRefusal::CodeCacheAtCapacity` and
//! the refusal counter — had **zero** references outside their own
//! implementation, five documentation files and one nightly workflow axis. No
//! unit test, no integration test, no assertion anywhere that the refusal is
//! reached, that it is a REFUSAL rather than a failure, or that it stops once
//! the pressure is gone.
//!
//! That matters more than an uncovered accessor usually does, because the path
//! is the one that must stay CORRECT — not merely slower — when the VM is
//! under memory pressure. `compile_gate::admit` refuses new compilations while
//! OSR trampolines and deopt stubs, which reach `ExecutableBuffer::new` by
//! other routes, keep allocating against the same cap.
//!
//! # Why the pressure is simulated rather than compiled
//!
//! `COMMITTED_JIT_CODE_BYTES` is the exact quantity the cap bounds, it is
//! public, and `jit_code_cache_at_capacity` reads nothing else. Driving it
//! directly tests the decision; compiling 256 MiB of real method bodies would
//! test the allocator, take minutes, and still only reach the same comparison.
//!
//! The alternative — setting `CRATONVM_JIT_CODE_CACHE_MAX_MB` to something
//! small — is deliberately NOT done. It is a declared flag, so it is served
//! from the latched snapshot rather than from `std::env`, and `set_var` on a
//! declared name is what `types/tests/flag_env_mutation_guard.rs` exists to
//! catch. The default cap is a documented constant and is asserted here
//! instead.
//!
//! # ONE `#[test]`, on purpose
//!
//! `COMMITTED_JIT_CODE_BYTES`, the cap's one-time warning flag and the refusal
//! counter are all process-global, and cargo runs a binary's tests as parallel
//! THREADS. A second test here would race this one's simulated pressure — and
//! while the pressure is up, any concurrent compile in the same binary is
//! refused. This file deliberately contains no codegen for that to disturb;
//! keep it that way, exactly as `gpu_barrier_arming.rs` does for the same
//! reason.
//!
//! # What is NOT covered here, and where the sweeper now lives
//!
//! When this file was written there was no sweeper at all, so exhaustion was a
//! permanent cliff: a long-running application that loads and unloads
//! generations of classes accumulated a full cache of live-but-cold bodies, and
//! the first method to go hot afterwards was refused FOREVER while code that
//! had not executed in an hour stayed mapped. The last assertion below pins the
//! property a sweeper needs — that admission resumes the moment the committed
//! total falls — and that is still what it is for.
//!
//! The eviction half landed as `JitCache::sweep_cold_bodies` (RT-8), which is
//! per-VM and therefore not reachable from this function: `jit_code_cache_at_capacity`
//! has no `JitCache` and there is no registry of live ones. It is driven from
//! `vm/src/runtime/interpreter/jit_bridge.rs`, off
//! `jit_code_cache_under_pressure()`, and gated behind
//! `CRATONVM_JIT_CODE_CACHE_SWEEP` (default off) AND
//! `CRATONVM_JIT_ENTRY_COUNTER` (its only usage signal; default ON since round 9
//! wave 3, so the sweep flag alone is what keeps the evictor off). Its
//! policy is tested in `jit/src/lib.rs`'s `code_cache_sweep_tests`, over
//! synthetic censuses rather than real artifacts — see that module for why an
//! end-to-end test of an evictor proves very little when it passes.
//!
//! So the refusals asserted below remain exactly the production behaviour of a
//! default configuration, which is what this file is about.

use cratonvm_jit::compile_gate::{self, CompileDoor, CompileRefusal};
use cratonvm_jit::COMMITTED_JIT_CODE_BYTES;
use std::sync::atomic::Ordering;

/// A class identity no real class has, so the per-class bail-list cannot have
/// a verdict about it.
const TEST_CLASS: cratonvm_types::ClassId = cratonvm_types::ClassId::new(0x00CA_CE01);

fn admit_once(door: CompileDoor) -> Result<(), CompileRefusal> {
    // `None` for the de-speculation registry: this test is about the code-cache
    // cap, and `TEST_CLASS` is an identity no real class has, so there is no
    // per-VM runtime verdict to consult. `None` means "nothing has de-spec'd",
    // which is the state that lets the cap be the only thing refusing.
    compile_gate::admit(
        TEST_CLASS,
        "cratonvm/test/CodeCacheCap",
        "hot",
        "()V",
        door,
        None,
    )
    .map(|admission| {
        // The token must be dropped explicitly rather than leaked: its `Drop`
        // closes the compilation scope this admission opened.
        drop(admission);
    })
}

#[test]
fn the_code_cache_cap_refuses_while_it_is_hit_and_admits_again_once_it_is_not() {
    // The cap this process is actually working to. Asserted rather than
    // assumed: a run with `CRATONVM_JIT_CODE_CACHE_MAX_MB` exported would be
    // measuring a different number, and `0` disables the cap entirely — in
    // which case there is nothing here to test and saying so is better than
    // passing vacuously.
    let cap = cratonvm_jit::jit_code_cache_cap_bytes();
    if cap == usize::MAX {
        eprintln!(
            "CRATONVM_JIT_CODE_CACHE_MAX_MB=0 disables the cap; this test has \
             nothing to assert in that configuration"
        );
        return;
    }
    assert_eq!(
        cap,
        256 * 1024 * 1024,
        "the default cap is 256 MiB, chosen to sit beside HotSpot's ~240 MiB \
         ReservedCodeCacheSize. A different number here means either the \
         constant moved or this process has the env var set, and the \
         assertions below are about the wrong threshold either way"
    );

    // A process that has compiled nothing is nowhere near the cap.
    let committed_at_entry = COMMITTED_JIT_CODE_BYTES.load(Ordering::Relaxed);
    assert!(
        committed_at_entry < cap,
        "this binary contains no codegen, so it must start well below the cap; \
         {committed_at_entry} bytes are committed"
    );
    // The capacity check is the LAST rung of `admit`'s ladder, so a process
    // that refuses earlier — `CRATONVM_DISABLE_JIT`, a `CRATONVM_JIT_DENY`
    // pattern that happens to match — would never reach it and every
    // assertion below would be about the wrong verdict. Say so and stop,
    // rather than passing vacuously or failing for an unrelated reason.
    match admit_once(CompileDoor::MethodEntry) {
        Ok(()) => {}
        Err(early) => {
            eprintln!(
                "this process refuses compilation before the capacity check is \
                 reached ({early:?}); the cap has nothing to be asked here"
            );
            return;
        }
    }

    // Simulate a full cache. `>=`, not `>`: the check is `used < cap`, so
    // exactly AT the cap must already refuse. That boundary is the one a
    // future sweeper's high-water mark would be written against.
    let refusals_before = cratonvm_jit::jit_code_cache_cap_refusals();
    COMMITTED_JIT_CODE_BYTES.store(cap, Ordering::Relaxed);

    // Every door, because the capacity check sits in the shared ladder and
    // nothing else says so. An OSR compile is a LOOP in a method that is
    // already running; if some future edit made the check door-dependent,
    // this is where it would show.
    for door in CompileDoor::ALL {
        assert_eq!(
            admit_once(door),
            Err(CompileRefusal::CodeCacheAtCapacity),
            "the {} door must refuse at the cap, and must refuse with the \
             TRANSIENT verdict: a caller distinguishes it from \
             PermanentlyBailListed, which is forever",
            door.label()
        );
    }
    assert_eq!(
        cratonvm_jit::jit_code_cache_cap_refusals(),
        refusals_before + CompileDoor::ALL.len() as u64,
        "each refusal must be counted exactly once; the counter is the only \
         evidence a production run leaves that it entered the degraded regime"
    );

    // The drain-and-retry arm. There is nothing on the retirement queue in
    // this binary, so what is asserted is the precondition the arm reads —
    // a non-zero queue length is what makes it worth draining, and a zero one
    // must not send the check down a path that reports success it did not
    // achieve.
    assert_eq!(
        cratonvm_jit::jit_retirement_queue_len(),
        0,
        "no artifact was ever installed here, so nothing can be queued; a \
         non-zero length would mean this test is sharing a process with \
         something that compiles"
    );
    assert_eq!(
        admit_once(CompileDoor::MethodEntry),
        Err(CompileRefusal::CodeCacheAtCapacity),
        "with an empty retirement queue there is nothing to reclaim, so the \
         refusal must stand rather than the drain being mistaken for headroom"
    );

    // One byte under the cap admits again. This is the property a sweeper
    // depends on and the reason the cliff is a POLICY failure rather than a
    // mechanism one: nothing about the refusal is sticky, so evicting cold
    // bodies would restore compilation immediately. What is missing is the
    // eviction, not the recovery.
    COMMITTED_JIT_CODE_BYTES.store(cap - 1, Ordering::Relaxed);
    assert_eq!(
        admit_once(CompileDoor::MethodEntry),
        Ok(()),
        "the refusal must be transient: one byte of headroom is enough, which \
         is exactly what a sweeper would be able to produce"
    );

    // Leave the process as it was found. Nothing else runs in this binary, so
    // this is tidiness rather than load-bearing — but a second test added here
    // (don't) would depend on it, and so would anything that later compiles.
    COMMITTED_JIT_CODE_BYTES.store(committed_at_entry, Ordering::Relaxed);
}
