// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `gpu_barrier::arm` and the two readers that consult what it published.
//!
//! # Why this is a separate binary
//!
//! `arm` writes two process-global atomics, and both compiled backends read
//! them at EMIT time (`x64/arrays.rs`, `ir_lower.rs`). So a test that arms
//! them does not only race other arming tests — while it is armed, every
//! other thread in the same binary that compiles a primitive array store
//! emits 51 bytes it should not.
//!
//! That is what went wrong until 2026-09-05. Four unit tests in
//! `jit/src/gpu_barrier.rs` armed, encoded and restored; cargo runs a
//! binary's tests as parallel threads, so a DIFFERENT pair of them failed on
//! roughly one run in three of `cargo test -p cratonvm-jit --lib`, and the
//! whole suite was green under `--test-threads=1`. Those four now use
//! `barrier_bytes_for` / `pair_is_armed` and touch no global state.
//!
//! The globals still need covering — otherwise `arm` has no test at all and
//! `barrier_bytes` could stop reading what it publishes with every gate
//! green. An integration test is its own PROCESS, and this file deliberately
//! contains no codegen test for the arming to disturb. Keep it that way: a
//! test added here that compiles anything reintroduces the bug in a new
//! place.
//!
//! ONE `#[test]` on purpose. Two would be threads again, in this binary.

use cratonvm_jit::gpu_barrier;

#[test]
fn arming_publishes_the_pair_that_the_readers_answer_with() {
    const FILTER: usize = 0x0000_7FF0_0020_0000;
    const DIRTY: usize = 0x0000_7FF0_0020_0040;

    // A fresh process has published nothing. Asserted, not assumed: if some
    // future `lazy_static`-style initialiser armed the barrier on first
    // touch, every claim below would be about the wrong pair.
    assert!(
        !gpu_barrier::is_armed(),
        "a fresh process must start unarmed"
    );
    assert!(
        gpu_barrier::barrier_bytes().is_none(),
        "unarmed emits nothing at all -- this is the byte-identical-codegen \
         promise for a CPU-only run"
    );

    gpu_barrier::arm(FILTER, DIRTY);

    // The wiring: both readers must answer for the pair just published,
    // not for a stale snapshot or a different pair.
    assert!(gpu_barrier::is_armed());
    let published = gpu_barrier::barrier_bytes().expect("armed");
    assert_eq!(
        published,
        gpu_barrier::barrier_bytes_for(FILTER, DIRTY).expect("armed pair"),
        "`barrier_bytes` must encode exactly the published pair"
    );
    assert_eq!(published.len(), gpu_barrier::BARRIER_LEN);

    // Re-arming is idempotent for the same pair, which is the property the
    // "safe to call from several VMs in one process" claim rests on.
    gpu_barrier::arm(FILTER, DIRTY);
    assert_eq!(
        gpu_barrier::barrier_bytes().as_deref(),
        Some(&published[..])
    );

    // Publishing a different pair is visible to the readers -- without this
    // the two assertions above would also pass against a `barrier_bytes`
    // that had memoized its first answer.
    const OTHER: usize = 0x0000_7FF0_0030_0000;
    gpu_barrier::arm(OTHER, DIRTY);
    assert_eq!(
        gpu_barrier::barrier_bytes().expect("armed"),
        gpu_barrier::barrier_bytes_for(OTHER, DIRTY).expect("armed pair"),
    );
    assert_ne!(
        gpu_barrier::barrier_bytes().expect("armed"),
        published,
        "a re-arm with a new filter address must change the emitted bytes"
    );

    // Half-publishing disarms: `offload_jit_gate` reads `is_armed` to pick
    // its array-writer policy, so the predicate and the encoder have to
    // stand down together.
    gpu_barrier::arm(FILTER, 0);
    assert!(!gpu_barrier::is_armed());
    assert!(gpu_barrier::barrier_bytes().is_none());

    // Leave the process as we found it. Nothing else runs in this binary,
    // so this is tidiness rather than load-bearing -- but a second test
    // added here (don't) would depend on it.
    gpu_barrier::arm(0, 0);
    assert!(!gpu_barrier::is_armed());
}
