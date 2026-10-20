// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcj j2/rf2 (2026-10-19) — ec47, `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`
//! (default ON, `=0` kills): a Generational major marks the finalizer seeds it
//! used to root (the true-root seed's candidates, a caller's
//! `OldGenFinalizerPass::deferred_seeds`) in the resurrection round, so the OLD
//! children of a dead YOUNG finalizable land in the depth-2 set
//! (`OldGenFinalizerPass::closure`) and their weak references are cleared
//! before `finalize()`, as HotSpot clears them
//! (`docs/internal/gc/gcj-j1rf-gen-major-roots-the-old-children-of-young-finalizables-FIXED-20261020.md`).
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gcj_j2rf2_`.

use super::*;

/// Test-only `StopTheWorldToken` (the harness runs the heap single-threaded).
fn stw() -> crate::collector::StopTheWorldToken {
    // SAFETY: these unit tests run the heap single-threaded.
    unsafe { crate::collector::StopTheWorldToken::new() }
}

struct NoMonitors;
impl crate::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

const DEFER: &str = "CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS";

/// The flags every case pins: the true-root seed at its defaults, no
/// requested-major compaction (the addresses must hold after the sweep), and
/// this lane's switch.
fn flags(defer: bool) -> [(&'static str, Option<&'static str>); 6] {
    [
        (DEFER, Some(if defer { "1" } else { "0" })),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS", None),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE", None),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD", None),
        ("CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG", None),
        ("CRATONVM_GC_REQUESTED_MAJOR_COMPACTS", Some("0")),
    ]
}

/// `n` tenured objects of two slots each, on a fresh heap.
fn tenured(n: usize) -> (GenerationalHeap, Vec<ObjectRef>) {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024).with_fixed_promotion_age();
    let monitors = NoMonitors;
    let mut roots: Vec<ObjectRef> = (0..n)
        .map(|_| heap.alloc_object(ClassId::new(0), 2))
        .collect();
    for _ in 0..PROMOTION_AGE {
        heap.collect_garbage(&stw(), &mut roots, &monitors);
    }
    assert!(
        roots.iter().all(|r| heap.is_in_old(r.as_ptr())),
        "precondition: tenured"
    );
    (heap, roots)
}

/// What one requested true-root major reported, by fixture index.
#[derive(Debug, PartialEq, Eq)]
struct Out {
    alive: Vec<bool>,
    resurrected: Vec<usize>,
    closure: Vec<usize>,
    true_root_majors: u64,
}

/// Fixture (indices of `Out`): 0 `child` (old) -> 1 `grand` (old); 2 `fin2`
/// (old, a candidate only `fin` names); 3 garbage (old); 4 `fin` (YOUNG, the
/// candidate, `fin.0 = child`, `fin.1 = fin2`). `fin_rooted`: `fin` is also a
/// root (a live finalizable).
fn run(defer: bool, fin_rooted: bool) -> Out {
    let (heap, old) = tenured(4);
    let (child, grand, fin2, garbage) = (old[0], old[1], old[2], old[3]);
    let fin = heap.alloc_object(ClassId::new(0), 2);
    assert!(!heap.is_in_old(fin.as_ptr()), "precondition: fin is young");
    heap.set_field(child, 0, Value::Object(Some(grand)));
    heap.set_field(fin, 0, Value::Object(Some(child)));
    heap.set_field(fin, 1, Value::Object(Some(fin2)));
    let cands: Vec<usize> = vec![fin.as_ptr() as usize, fin2.as_ptr() as usize];
    let mut pass = OldGenFinalizerPass::new(&cands);
    let roots: Vec<ObjectRef> = if fin_rooted { vec![fin] } else { Vec::new() };
    cratonvm_types::flags::with_thread_overrides(&flags(defer), || {
        let _armed = TrueRootMajorScope::arm(true);
        let _ = heap.sweep_old_gen_non_moving_finalizing(
            &roots,
            &cratonvm_types::PointerMap::default(),
            &mut pass,
        );
    });
    let fixture = [child, grand, fin2, garbage, fin];
    let og = heap.old_gen.lock();
    let alive: Vec<bool> = fixture[..4]
        .iter()
        .map(|r| og.is_allocated_addr(r.as_ptr()))
        .collect();
    let true_root_majors = og.sizing_stats().true_root_majors;
    drop(og);
    let addrs = fixture.map(|o| o.as_ptr() as usize);
    let index = |a: &usize| {
        addrs
            .iter()
            .position(|f| f == a)
            .unwrap_or_else(|| panic!("0x{a:x} is not a fixture object: {addrs:x?}"))
    };
    let mut resurrected: Vec<usize> = pass.resurrected.iter().map(index).collect();
    resurrected.sort_unstable();
    let mut closure: Vec<usize> = pass.closure.iter().map(index).collect();
    closure.sort_unstable();
    Out {
        alive,
        resurrected,
        closure,
        true_root_majors,
    }
}

/// The switch is default ON and `=0` kills it.
#[test]
fn gcj_j2rf2_major_defers_finalizer_seeds_is_default_on_and_0_kills_it() {
    let read = |v: Option<&'static str>| {
        cratonvm_types::flags::with_thread_overrides(
            &[(DEFER, v)],
            gen_major_defers_finalizer_seeds_enabled,
        )
    };
    assert!(read(None), "unset: on");
    assert!(read(Some("1")));
    assert!(!read(Some("0")), "=0 kills");
}

/// THE REGRESSION (ec47). A dead young finalizable's old child and grandchild
/// are kept (both arms) and, with the switch, named by the depth-2 set; the
/// old candidate only the dead young one reaches is finalized at this pause
/// too (HotSpot's keep-alive phase enqueues both). The kill arm is the
/// pre-fix verdict: the old subgraph is strongly marked, so the depth-2 set
/// is empty and `fin2` is not resurrected. The garbage is freed on both.
#[test]
fn gcj_j2rf2_old_children_of_a_dead_young_finalizable_are_the_depth_two_set() {
    let on = run(true, false);
    assert_eq!(on.true_root_majors, 1, "precondition: a true-root major: {on:?}");
    assert_eq!(on.alive, vec![true, true, true, false], "{on:?}");
    assert_eq!(on.resurrected, vec![2], "fin2 is dead and finalized now: {on:?}");
    assert_eq!(on.closure, vec![0, 1, 2], "child, grand, fin2: {on:?}");

    let off = run(false, false);
    assert_eq!(off.true_root_majors, 1, "{off:?}");
    assert_eq!(off.alive, vec![true, true, true, false], "the same marks: {off:?}");
    assert!(off.resurrected.is_empty(), "kill arm: rooted by fin: {off:?}");
    assert!(off.closure.is_empty(), "kill arm: strongly marked: {off:?}");
}

/// The guard: a LIVE young finalizable's old subgraph is strongly reachable,
/// so the deferral names none of it (the depth-2 set must never name a
/// strongly reachable object) and nothing is resurrected.
#[test]
fn gcj_j2rf2_a_live_young_finalizables_old_children_stay_strong() {
    for defer in [true, false] {
        let out = run(defer, true);
        assert_eq!(out.true_root_majors, 1, "defer={defer}: {out:?}");
        assert_eq!(out.alive, vec![true, true, true, false], "defer={defer}: {out:?}");
        assert!(out.resurrected.is_empty(), "defer={defer}: {out:?}");
        assert!(out.closure.is_empty(), "defer={defer}: {out:?}");
    }
}

/// `deferred_seeds` is honoured whatever the switch reads in the callee and
/// with no candidate at all (the round is due on its own): the deferred old
/// object and its referent are kept and named by the depth-2 set, on the
/// true-root and on the legacy seed. Unmarked, they would be freed under the
/// finalizer row that asked for them.
#[test]
fn gcj_j2rf2_deferred_seeds_are_always_marked_by_the_round() {
    for (defer, true_roots) in [(true, true), (false, true), (true, false), (false, false)] {
        let (heap, old) = tenured(3);
        let (seed, child, garbage) = (old[0], old[1], old[2]);
        heap.set_field(seed, 0, Value::Object(Some(child)));
        let mut pass = OldGenFinalizerPass::new(&[]);
        pass.deferred_seeds.push(seed.as_ptr() as usize);
        cratonvm_types::flags::with_thread_overrides(&flags(defer), || {
            let _armed = TrueRootMajorScope::arm(true_roots);
            let _ = heap.sweep_old_gen_non_moving_finalizing(
                &[],
                &cratonvm_types::PointerMap::default(),
                &mut pass,
            );
        });
        let og = heap.old_gen.lock();
        let tag = format!("defer={defer} true_roots={true_roots}");
        assert!(og.is_allocated_addr(seed.as_ptr()), "{tag}: the seed was freed");
        assert!(og.is_allocated_addr(child.as_ptr()), "{tag}: its referent was freed");
        assert!(!og.is_allocated_addr(garbage.as_ptr()), "{tag}: garbage kept");
        drop(og);
        let mut closure = pass.closure.clone();
        closure.sort_unstable();
        let mut want = vec![seed.as_ptr() as usize, child.as_ptr() as usize];
        want.sort_unstable();
        assert_eq!(closure, want, "{tag}");
        assert!(pass.deferred_seeds.is_empty(), "{tag}: taken by the round");
        assert!(pass.resurrected.is_empty(), "{tag}: not a candidate");
    }
}
