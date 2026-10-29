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

    // Disarm before the RT-7 block below, which has to start from unarmed.
    gpu_barrier::arm(0, 0);
    assert!(!gpu_barrier::is_armed());

    // -------------------------------------------------------------------
    // RT-7 — arming is process-global, compiled code is per-VM.
    // -------------------------------------------------------------------
    //
    // `arm`'s "before any method can be compiled" is true of the VM that
    // calls it and says nothing about the PROCESS. An embedding that creates
    // VM #1 without `--gpu` — compiling array writers with no barrier — and
    // only then VM #2 with `--gpu` used to flip `array_writer_policy` for the
    // whole process while VM #1's already-compiled writers went on marking no
    // dirty bucket.
    //
    // RT-7 made that POLICY per-VM (`array_writer_policy` reads
    // `SharedVm::gpu_array_barrier_armed`, `arm`'s return value on the VM that
    // called it), and on 2026-09-17 removed the REFUSAL that used to stand in
    // for it. `gpu_barrier::arm`'s doc carries the proof: the residency cache's
    // table is per-VM, every VM identity owns a distinct heap, identities are
    // never reused, and a VM with a cache arms before its first compile -- so
    // the barrier-less stores committed before a late arming write only heaps
    // no cache table references.
    //
    // What this file now pins is the CONTRACT the refusal used to break: a late
    // arming publishes and reports armed, so the VM that calls it gets its
    // compiled array-writing path instead of `KeepCache`.
    //
    // `COMMITTED_JIT_CODE_BYTES` is the process's own record of that, and is
    // written here directly rather than by compiling something: this binary
    // deliberately contains no codegen (see the module doc above), and an
    // `ExecutableBuffer` allocated to make the number move would be exactly
    // what that doc forbids.
    use std::sync::atomic::Ordering;
    cratonvm_jit::COMMITTED_JIT_CODE_BYTES.store(4096, Ordering::Relaxed);

    assert!(
        gpu_barrier::arm(FILTER, DIRTY),
        "a LATE arming -- code already committed in this process -- must succeed \
         and report armed. Until 2026-09-17 it was refused, which left a \
         late-created --gpu VM's array writers interpreted for no safety benefit"
    );
    assert!(
        gpu_barrier::is_armed(),
        "and it must actually publish: the emitter reads these globals"
    );
    assert_eq!(
        gpu_barrier::barrier_bytes(),
        gpu_barrier::barrier_bytes_for(FILTER, DIRTY),
        "the published pair is the one armed",
    );

    // A disarm reports what it did. `arm` now answers "is the barrier armed
    // after this call" -- the fact the caller records -- rather than "was this
    // call accepted", so a disarm answers `false`.
    assert!(!gpu_barrier::arm(0, 0));
    assert!(!gpu_barrier::is_armed());

    // A clean-process arming and a RE-ARM while already armed -- the ordinary
    // single-VM startup and the several-VMs case. Both always succeeded and
    // still do; kept so removing the refusal is seen not to have changed them.
    cratonvm_jit::COMMITTED_JIT_CODE_BYTES.store(0, Ordering::Relaxed);
    assert!(gpu_barrier::arm(FILTER, DIRTY), "a clean process may arm");
    cratonvm_jit::COMMITTED_JIT_CODE_BYTES.store(4096, Ordering::Relaxed);
    assert!(gpu_barrier::arm(OTHER, DIRTY), "a re-arm must go through");
    assert!(gpu_barrier::is_armed());

    // Leave the process as we found it. Nothing else runs in this binary,
    // so this is tidiness rather than load-bearing -- but a second test
    // added here (don't) would depend on it.
    cratonvm_jit::COMMITTED_JIT_CODE_BYTES.store(0, Ordering::Relaxed);
    assert!(!gpu_barrier::arm(0, 0));
    assert!(!gpu_barrier::is_armed());
}
