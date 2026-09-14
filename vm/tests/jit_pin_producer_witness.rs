// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! **ZGC's conservative-JIT-root pin has a consumer and no producer.** That is
//! survivable only while ZGC declines to relocate at all under a live compiled
//! frame. This test pins the two facts together so the second cannot be relaxed
//! without noticing the first.
//!
//! `gc_quiescence::pinned_jit_roots_snapshot()` is two-sided: a CONSUMER (a
//! moving collector dropping those pages/regions from what it is about to
//! relocate) and a PRODUCER (the VM's root deposits filling the registry).
//! `caf25c3d1` (2026-08-14) gave ZGC the consumer. All four producers —
//! `memory/roots.rs`, `update_root_snapshot`, the blocked-thread deposit in
//! `vm/vm_exec.rs`, and `pin_frozen_peer_roots_for_g1` — are still gated on
//! `shared.mem.heap.is_g1()`, so under ZGC that snapshot is **empty on every
//! cycle** and the consumer withholds nothing.
//!
//! Nothing behavioural catches that, because the two sides fail independently:
//! `zgc.rs`'s `a_conservative_jit_root_pins_its_page_against_relocation` calls
//! `add_pinned_jit_root` itself, so it exercises the consumer over a snapshot
//! the VM would never have produced — and passes with every producer deleted.
//!
//! What makes it safe today is `3c0fd9c01`: ZGC reads
//! `gc_quiescence::is_active()` and refuses to relocate while any compiled frame
//! is live. That is strictly stronger than the pin, and deliberately so — a pin
//! built from a CONSERVATIVE STACK SCAN cannot see a pointer that never left a
//! register, so the object slides anyway. Wiring the producers up was measured
//! to fix `probes/JitFrameRootRelocationProbe`, and was still dropped in favour
//! of the refusal for exactly that reason.
//!
//! But that commit files its own caveat: *"a JIT-busy process compacts less
//! often, and on this collector compaction is also defragmentation […] Someone
//! should measure the fragmentation impact on a JIT-heavy workload before this
//! is considered settled."* If that refusal is narrowed or removed, the pin
//! becomes ZGC's remaining protection and it has no producer. This test is the
//! tripwire on that path — not a style check.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The refusal is what makes the missing producer survivable. If it goes, read
/// this test's header before deciding the pin is enough.
#[test]
fn zgc_declines_to_relocate_while_a_compiled_frame_is_live() {
    let zgc = read("gc/src/zgc.rs");
    assert!(
        zgc.contains("gc_quiescence::is_active"),
        "gc/src/zgc.rs no longer asks `gc_quiescence::is_active()`. That refusal \
         (3c0fd9c01) is the ONLY thing keeping a page slide off an object held \
         in a compiled frame's register — the conservative-root pin cannot \
         substitute, because a pointer that never left a register is absent \
         from the scan the pin is built from. If this is being narrowed on \
         purpose, wire up the pin PRODUCERS in the same commit (they are all \
         still `is_g1()`; see this file's header) and re-run \
         probes/JitFrameRootRelocationProbe, which reproduces the defect \
         deterministically at iteration 10191."
    );
}

/// The consumer half, stated so the pair cannot be half-deleted.
#[test]
fn both_moving_collectors_consume_the_pinned_jit_root_snapshot() {
    for file in ["gc/src/g1.rs", "gc/src/zgc.rs"] {
        let src = read(file);
        assert!(
            src.contains("pinned_jit_roots_snapshot"),
            "{file} no longer consumes `pinned_jit_roots_snapshot()`. For G1 that \
             is a live memory-corruption bug; for ZGC it removes the second \
             layer under the `is_active()` refusal."
        );
    }
}

/// And the producer half, as a fact rather than an assertion of correctness:
/// it records that all four publishers are G1-only, so a reader who finds this
/// test red has been told what changed rather than left to diff four files.
#[test]
fn the_pin_producers_are_still_g1_only() {
    let sites = [
        ("vm/src/memory/roots.rs", "add_pinned_jit_root"),
        (
            "vm/src/runtime/interpreter/gc_and_alloc.rs",
            "publish_pinned_jit_roots",
        ),
        ("vm/src/vm/vm_exec.rs", "publish_pinned_jit_roots"),
    ];
    for (file, call) in sites {
        let src = read(file);
        assert!(
            src.contains(call),
            "{file} no longer calls {call} — if the producer moved, move this \
             witness with it"
        );
        assert!(
            src.contains("is_g1()"),
            "{file} publishes conservative JIT roots ({call}) and no longer gates \
             on `is_g1()`. If ZGC now produces them too, that is a CHANGE OF \
             POLICY, not a cleanup: read this file's header, then delete this \
             test and restore the behavioural one it replaced."
        );
    }
}
