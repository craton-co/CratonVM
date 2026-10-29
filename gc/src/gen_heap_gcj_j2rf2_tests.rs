// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcj j2/rf2 (2026-10-19) — ec47, `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`
//! (default ON, `=0` kills): a Generational major marks the finalizer seeds it
//! used to root (the true-root seed's candidates, a caller's
//! `OldGenFinalizerPass::deferred_seeds`) in the resurrection round, so the OLD
//! children of a dead YOUNG finalizable land in the depth-2 set
//! (`OldGenFinalizerPass::closure`) and their weak references are cleared
//! before `finalize()`, as HotSpot clears them
//! (`docs/feature-designs/generative-gc-architecture.md`).
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
fn flags(defer: bool) -> [(&'static str, Option<&'static str>); 7] {
    flags_with(defer, true)
}

/// [`flags`] with gcj j4/rf's `CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG` pinned.
fn flags_with(defer: bool, records_young: bool) -> [(&'static str, Option<&'static str>); 7] {
    [
        (DEFER, Some(if defer { "1" } else { "0" })),
        (
            "CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG",
            Some(if records_young { "1" } else { "0" }),
        ),
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
    run_with(defer, fin_rooted, true)
}

/// [`run`] with `CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG` pinned (gcj j4/rf).
fn run_with(defer: bool, fin_rooted: bool, records_young: bool) -> Out {
    let (heap, old) = tenured(4);
    let (child, grand, fin2, garbage) = (old[0], old[1], old[2], old[3]);
    let fin = heap.alloc_object(ClassId::new(0), 2);
    assert!(!heap.is_in_old(fin.as_ptr()), "precondition: fin is young");
    heap.set_field(child, 0, Value::Object(Some(grand)));
    heap.set_field(fin, 0, Value::Object(Some(child)));
    heap.set_field(fin, 1, Value::Object(Some(fin2)));
    let cands: Vec<usize> = vec![fin.as_ptr() as usize, fin2.as_ptr() as usize];
    // gcj j3/x2 R3: the pass latches the switch when it is built, so it is
    // built under the case's flags.
    let mut pass =
        cratonvm_types::flags::with_thread_overrides(&flags_with(defer, records_young), || {
            OldGenFinalizerPass::new(&cands)
        });
    let roots: Vec<ObjectRef> = if fin_rooted { vec![fin] } else { Vec::new() };
    cratonvm_types::flags::with_thread_overrides(&flags_with(defer, records_young), || {
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
    // gcj j4/rf (`CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG`, default ON): the
    // round also records the young objects it reaches -- here `fin` itself,
    // which the round marks from its deferred seeds (`=0`: old pops only).
    assert_eq!(
        on.closure,
        vec![0, 1, 2, 4],
        "child, grand, fin2, fin: {on:?}"
    );
    let old_pops_only = run_with(true, false, false);
    assert_eq!(old_pops_only.closure, vec![0, 1, 2], "{old_pops_only:?}");

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
        let mut pass = cratonvm_types::flags::with_thread_overrides(&flags(defer), || {
            OldGenFinalizerPass::new(&[])
        });
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

// ---------------------------------------------------------------------------
// gcj j3/x2 R2 + R3: the MOVING-path hunk of ec47 (`d24c24efc0`) -- Phase 5's
// `finalizers_in_old` / P-C seeds deferred to the major's resurrection round.
// ---------------------------------------------------------------------------

/// What one moving cycle with a same-pause occupancy major reported.
#[derive(Debug)]
struct MovingOut {
    /// The old child is still allocated.
    child_alive: bool,
    /// The finalizable's promotion destination is still allocated.
    fin_dest_alive: bool,
    /// `take_resurrection_closure()` names the child.
    closure_has_child: bool,
    /// ... names the finalizable's PRE-collection (young) address.
    closure_has_fin_pre: bool,
    /// ... names its post-copy destination (the R2 defect).
    closure_has_fin_dest: bool,
    /// The finalizable was reported dead (at its destination).
    fin_reported: bool,
}

/// A tenured old child `child`, a young finalizable `fin` (`fin.0 = child`,
/// no root), ballast to 3/4 of old gen so the moving cycle runs its Phase 5
/// major by occupancy (no process-wide `System.gc()` request), and
/// `finalizer_addrs = [fin]`. Every survivor is promoted, so `fin`'s
/// resurrected copy is old (`finalizers_in_old`). `pc`: the P-C (true-root)
/// moving major; otherwise the legacy all-young seed.
fn moving_major(defer: bool, pc: bool) -> MovingOut {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 16 * 1024);
    let monitors = NoMonitors;
    let flags: [(&str, Option<&str>); 7] = [
        (DEFER, Some(if defer { "1" } else { "0" })),
        ("CRATONVM_GC_OLD_TRIGGER_HYSTERESIS", Some("0")),
        ("CRATONVM_OLDGEN_COMPACT", Some("0")),
        ("CRATONVM_GC_REQUESTED_MAJOR_COMPACTS", Some("0")),
        ("CRATONVM_GEN_PINNED_YOUNG_COPY", Some("0")),
        (
            "CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS",
            Some(if pc { "1" } else { "0" }),
        ),
        ("CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG", None),
    ];
    cratonvm_types::flags::with_thread_overrides(&flags, || {
        let probe = heap.alloc_object(ClassId::new(1), 2);
        let obj_size = gen_object_total_size(heap.get_header(probe));
        let old_cap = heap.old_gen_capacity();
        let n = (old_cap * 3 / 4) / obj_size + 1;
        assert!(
            (n + 4) * obj_size < old_cap,
            "precondition: the fixture fits old gen"
        );
        let mut roots: Vec<ObjectRef> = (0..n)
            .map(|_| heap.alloc_object(ClassId::new(1), 2))
            .collect();
        roots.push(heap.alloc_object(ClassId::new(2), 2));
        heap.force_promote_all.store(true, Ordering::Relaxed);
        heap.collect_garbage(&stw(), &mut roots, &monitors);
        let child = roots.pop().expect("the child");
        assert!(
            heap.is_in_old(child.as_ptr()),
            "precondition: the child is old"
        );
        let fin = heap.alloc_object(ClassId::new(3), 2);
        assert!(!heap.is_in_old(fin.as_ptr()), "precondition: fin is young");
        heap.set_field(fin, 0, Value::Object(Some(child)));
        let fin_pre = fin.as_ptr() as usize;
        let child_addr = child.as_ptr() as usize;
        let _ = heap.take_resurrection_closure();
        heap.force_promote_all.store(true, Ordering::Relaxed);
        let majors_before = heap.stats().snapshot().major_gc_count;
        let (result, dead) =
            heap.collect_garbage_with_finalizers(&stw(), &mut roots, &[fin_pre], &monitors);
        assert!(
            heap.stats().snapshot().major_gc_count > majors_before,
            "precondition: this moving pause also ran its Phase 5 major"
        );
        let fin_dest = *result
            .pointer_map
            .get(&fin_pre)
            .expect("precondition: the dead young finalizable was resurrected (copied)");
        assert!(
            heap.is_in_old(fin_dest as *const u8),
            "precondition: promoted"
        );
        let closure = heap.take_resurrection_closure();
        let og = heap.old_gen.lock();
        MovingOut {
            child_alive: og.is_allocated_addr(child_addr as *const u8),
            fin_dest_alive: og.is_allocated_addr(fin_dest as *const u8),
            closure_has_child: closure.contains(&child_addr),
            closure_has_fin_pre: closure.contains(&fin_pre),
            closure_has_fin_dest: closure.contains(&fin_dest),
            fin_reported: dead.contains(&fin_dest),
        }
    })
}

/// R3, fail-first (reverting `d24c24efc0` turns the ON arm's
/// `closure_has_child` false): on the moving cycle the old child of a dead
/// young finalizable is the depth-2 set with the switch ON and strongly
/// marked with `=0`; both keep the child and the finalizable's copy
/// allocated (P-C and the legacy seed alike) and report it dead.
#[test]
fn gcj3x2_r3_the_moving_major_defers_the_promoted_finalizables_old_child() {
    for pc in [true, false] {
        let on = moving_major(true, pc);
        assert!(on.child_alive && on.fin_dest_alive, "pc={pc}: kept: {on:?}");
        assert!(on.fin_reported, "pc={pc}: finalized at this pause: {on:?}");
        assert!(on.closure_has_child, "pc={pc}: the depth-2 set: {on:?}");
        let off = moving_major(false, pc);
        assert!(
            off.child_alive && off.fin_dest_alive,
            "pc={pc}: kept: {off:?}"
        );
        assert!(off.fin_reported, "pc={pc}: {off:?}");
        assert!(
            !off.closure_has_child,
            "pc={pc}: kill arm, strongly marked: {off:?}"
        );
    }
}

/// R2: the closure carries PRE-collection addresses on the moving path too --
/// the promoted finalizable's copy the round popped is named by its young
/// source, never by its post-copy destination.
#[test]
fn gcj3x2_r2_the_moving_closure_names_sources_not_promotion_destinations() {
    for pc in [true, false] {
        let on = moving_major(true, pc);
        assert!(
            !on.closure_has_fin_dest,
            "pc={pc}: a post-copy address: {on:?}"
        );
        assert!(on.closure_has_fin_pre, "pc={pc}: the source: {on:?}");
    }
}

/// R2's mapping alone: old destinations become their sources, everything else
/// (an old object in place, a young source) is left as it is.
#[test]
fn gcj3x2_r2_map_promotion_destinations_to_sources_is_exact() {
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x1000, 0x9000); // promoted (old destination)
    map.insert(0x1100, 0x2100); // a survivor copy (young destination)
    let is_old = |a: usize| a >= 0x8000;
    let mut closure = vec![0x9000, 0x8800, 0x1100, 0x2100];
    map_promotion_destinations_to_sources(&mut closure, &map, is_old);
    assert_eq!(closure, vec![0x1000, 0x8800, 0x1100, 0x2100]);
    let mut empty: Vec<usize> = Vec::new();
    map_promotion_destinations_to_sources(&mut empty, &map, is_old);
    assert!(empty.is_empty());
}

/// R3: the pass latches the switch when it is built.
#[test]
fn gcj3x2_r3_the_pass_latches_the_switch_once() {
    for (v, want) in [(None, true), (Some("1"), true), (Some("0"), false)] {
        let pass = cratonvm_types::flags::with_thread_overrides(&[(DEFER, v)], || {
            OldGenFinalizerPass::new(&[])
        });
        assert_eq!(pass.defers_seeds, want, "{v:?}");
        // Read later under a different override: still the latched answer.
        let flipped = cratonvm_types::flags::with_thread_overrides(
            &[(DEFER, Some(if want { "0" } else { "1" }))],
            || pass.defers_seeds,
        );
        assert_eq!(flipped, want);
    }
}
