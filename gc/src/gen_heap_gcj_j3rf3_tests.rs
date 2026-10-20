// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcj j3/rf3 (2026-10-19) — ec47, the legacy seed's half
//! (`CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED`, default ON, `=0` kills;
//! engaged only with `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`): a major on
//! the legacy young seed (`mark_young_to_old_refs`) moves the old referents of
//! the young phase's resurrection set (`OldGenFinalizerPass::young_resurrected`)
//! to the resurrection round, so the OLD children of a dead YOUNG finalizable
//! land in the depth-2 set and their weak references are cleared before
//! `finalize()`, as HotSpot clears them (rf2 finding 3,
//! `docs/internal/gc/gcj-j1rf-gen-major-roots-the-old-children-of-young-finalizables-FIXED-20261020.md`).
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gcj_j3rf3_`.

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

const LEGACY: &str = "CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED";
const DEFER: &str = "CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS";

/// The flags every case pins: both switches, the true-root seed's knobs at
/// their defaults (the cases arm the LEGACY seed explicitly) and no
/// requested-major compaction (the addresses must hold after the sweep).
fn flags(defer: bool, legacy: bool) -> [(&'static str, Option<&'static str>); 6] {
    [
        (DEFER, Some(if defer { "1" } else { "0" })),
        (LEGACY, Some(if legacy { "1" } else { "0" })),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS", None),
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

/// What one legacy-seeded major reported, by fixture index.
#[derive(Debug, PartialEq, Eq)]
struct Out {
    alive: Vec<bool>,
    resurrected: Vec<usize>,
    closure: Vec<usize>,
    /// The run-total counter grew (only ever asserted TRUE: the counter is
    /// process-wide and other tests run in parallel).
    deferred_total_grew: bool,
}

/// Fixture (indices of `Out`): 0 `child` (old) -> 1 `grand` (old); 2 `fin2`
/// (old, a candidate only `fin` names); 3 garbage (old); 4 `fin` (YOUNG, the
/// candidate, `fin.0 = child`, `fin.1 = fin2`), never a root. `member`: `fin`
/// is in the young resurrection set handed to the major (what the young
/// phase's drain recorded for it).
fn run(defer: bool, legacy: bool, member: bool) -> Out {
    let (heap, old) = tenured(4);
    let (child, grand, fin2, garbage) = (old[0], old[1], old[2], old[3]);
    let fin = heap.alloc_object(ClassId::new(0), 2);
    assert!(!heap.is_in_old(fin.as_ptr()), "precondition: fin is young");
    heap.set_field(child, 0, Value::Object(Some(grand)));
    heap.set_field(fin, 0, Value::Object(Some(child)));
    heap.set_field(fin, 1, Value::Object(Some(fin2)));
    let cands: Vec<usize> = vec![fin.as_ptr() as usize, fin2.as_ptr() as usize];
    let mut pass = OldGenFinalizerPass::new(&cands);
    if member {
        pass.young_resurrected = vec![fin.as_ptr() as usize];
    }
    let before = LEGACY_SEED_DEFERRED_REFERENTS.load(Ordering::Relaxed);
    cratonvm_types::flags::with_thread_overrides(&flags(defer, legacy), || {
        let _armed = TrueRootMajorScope::arm(false);
        let _ = heap.sweep_old_gen_non_moving_finalizing(
            &[],
            &cratonvm_types::PointerMap::default(),
            &mut pass,
        );
    });
    let deferred_total_grew = LEGACY_SEED_DEFERRED_REFERENTS.load(Ordering::Relaxed) > before;
    let fixture = [child, grand, fin2, garbage, fin];
    let og = heap.old_gen.lock();
    let alive: Vec<bool> = fixture[..4]
        .iter()
        .map(|r| og.is_allocated_addr(r.as_ptr()))
        .collect();
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
    assert!(pass.deferred_seeds.is_empty(), "taken by the round");
    Out {
        alive,
        resurrected,
        closure,
        deferred_total_grew,
    }
}

/// The switch is default ON and `=0` kills it.
#[test]
fn gcj_j3rf3_legacy_seed_defers_resurrected_is_default_on_and_0_kills_it() {
    let read = |v: Option<&'static str>| {
        cratonvm_types::flags::with_thread_overrides(
            &[(LEGACY, v)],
            legacy_seed_defers_resurrected_enabled,
        )
    };
    assert!(read(None), "unset: on");
    assert!(read(Some("1")));
    assert!(!read(Some("0")), "=0 kills");
}

/// THE REGRESSION (rf2 finding 3). On the legacy seed, a dead young
/// finalizable in the resurrection set no longer marks its old subgraph
/// strongly: child and grandchild are kept and named by the depth-2 set, and
/// the old candidate only it reaches is finalized at this pause. The same
/// objects survive on every arm (the final mark is the same set).
#[test]
fn gcj_j3rf3_legacy_seed_old_children_of_a_dead_young_finalizable_are_depth_two() {
    let on = run(true, true, true);
    assert_eq!(on.alive, vec![true, true, true, false], "{on:?}");
    assert_eq!(on.resurrected, vec![2], "fin2 is dead and finalized now: {on:?}");
    assert_eq!(on.closure, vec![0, 1, 2], "child, grand, fin2: {on:?}");
    assert!(on.deferred_total_grew, "positive control: {on:?}");

    // The kill, and the dormant arm under rf2's kill: the pre-fix verdict.
    for (defer, legacy) in [(true, false), (false, true)] {
        let off = run(defer, legacy, true);
        let tag = format!("defer={defer} legacy={legacy}");
        assert_eq!(off.alive, vec![true, true, true, false], "{tag}: {off:?}");
        assert!(off.resurrected.is_empty(), "{tag}: rooted by fin: {off:?}");
        assert!(off.closure.is_empty(), "{tag}: strongly marked: {off:?}");
    }
}

/// The guard: a young object OUTSIDE the resurrection set (live, or garbage
/// the legacy seed retains) still seeds its old referents strongly, so the
/// depth-2 set never names what a strong path reaches.
#[test]
fn gcj_j3rf3_a_young_object_outside_the_resurrection_set_seeds_strongly() {
    let out = run(true, true, false);
    assert_eq!(out.alive, vec![true, true, true, false], "{out:?}");
    assert!(out.resurrected.is_empty(), "{out:?}");
    assert!(out.closure.is_empty(), "{out:?}");
}

/// The member set is the recorded PRE-collection keys at their current
/// addresses: a moved key maps through `pointer_map`, an unmapped one stays,
/// and the result is sorted and deduplicated (the walk binary-searches it).
#[test]
fn gcj_j3rf3_young_resurrection_members_map_through_the_pointer_map() {
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x1000, 0x9000);
    map.insert(0x2000, 0x2000);
    let got = young_resurrection_members_of(&[0x3000, 0x1000, 0x2000, 0x3000], &map);
    assert_eq!(got, vec![0x2000, 0x3000, 0x9000]);
    assert!(young_resurrection_members_of(&[], &map).is_empty());
}
