// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Source witness: **every publisher of a conservative JIT root asks the
//! collector-agnostic predicate, not `is_g1()`.**
//!
//! `gc_quiescence::pinned_jit_roots_snapshot()` is a two-sided contract. The
//! CONSUMER side (a collector dropping those pages/regions from what it is
//! about to move) and the PRODUCER side (the VM's root deposits filling the
//! registry) live in different crates, and on 2026-08-14 they named different
//! collectors for a day: `caf25c3d1` gave ZGC the consumer, while all four
//! producers stayed gated on `shared.mem.heap.is_g1()`. Under ZGC the snapshot
//! was therefore empty on every cycle and the filter dropped nothing — so the
//! pin read as *implemented* everywhere and *pinned* nowhere.
//!
//! Nothing behavioural could catch that cheaply, because the two sides fail
//! independently: `zgc.rs`'s
//! `a_conservative_jit_root_pins_its_page_against_relocation` calls
//! `add_pinned_jit_root` itself, so it exercises the consumer over a snapshot
//! the VM would never have produced, and passes with every producer removed.
//! `VmHeap::pins_conservative_jit_roots` is the single predicate both sides
//! now ask; this test is what stops a fifth producer being added — or one of
//! these four being edited — with `is_g1()` again.
//!
//! The observable defect it stands in for: one Java object in 200 000 came out
//! of a JIT-compiled constructor with a `null` `final` field. Its `this` was
//! live only in a compiled frame, ZGC's page slide moved it, the conservative
//! frame slot was not (and cannot be) rewritten, and the `putfield` landed in
//! the vacated span. The same defect crashed
//! `io.netty.util.collection.IntObjectHashMapTest` in `Monitor::exit` — see the
//! retired `intobjecthashmaptest-discovery-sigsegv-20260814` write-up.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // `vm/` -> repo root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The publishing call sites, and the file each lives in.
const PRODUCERS: &[(&str, &str)] = &[
    ("vm/src/memory/roots.rs", "add_pinned_jit_root"),
    (
        "vm/src/runtime/interpreter/gc_and_alloc.rs",
        "publish_pinned_jit_roots",
    ),
    (
        "vm/src/runtime/interpreter/gc_and_alloc.rs",
        "add_pinned_jit_root",
    ),
    ("vm/src/vm/vm_exec.rs", "publish_pinned_jit_roots"),
];

#[test]
fn every_conservative_jit_root_publisher_asks_the_shared_predicate() {
    for (file, call) in PRODUCERS {
        let src = read(file);
        assert!(
            src.contains(call),
            "{file} no longer calls {call} — if the producer moved, move this witness too"
        );
        assert!(
            src.contains("pins_conservative_jit_roots"),
            "{file} publishes conservative JIT roots ({call}) but never asks \
             `pins_conservative_jit_roots`. That predicate is the one place the \
             producer and the collector's relocation filter agree about which \
             backends move; gating on `is_g1()` here is what left ZGC's pin \
             registry empty on every cycle while its consumer was already live."
        );
    }
}

/// The consumer half, stated so the pair cannot be half-deleted: if a
/// collector stops reading the snapshot, this test is the reminder that its
/// producer arm in `pins_conservative_jit_roots` is now dead weight — and if a
/// new moving collector starts reading it, that it needs an arm at all.
#[test]
fn both_moving_collectors_consume_the_pinned_jit_root_snapshot() {
    for file in ["gc/src/g1.rs", "gc/src/zgc.rs"] {
        let src = read(file);
        assert!(
            src.contains("pinned_jit_roots_snapshot"),
            "{file} no longer consumes `pinned_jit_roots_snapshot()`. A moving \
             collector that ignores it relocates objects named only by an \
             un-rewritable JIT frame slot."
        );
    }
    let heap = read("gc/src/vm_heap.rs");
    assert!(
        heap.contains("pub fn pins_conservative_jit_roots"),
        "`VmHeap::pins_conservative_jit_roots` is the shared predicate both \
         sides ask; removing it re-opens the producer/consumer drift"
    );
}
