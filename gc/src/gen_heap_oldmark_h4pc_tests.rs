// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gch h4/pc (2026-10-09) — unit tests of the true-root young rounds' cost cut
//! (`gch-h3mj-proposal-true-root-rounds-rescan-every-marked-old-object-DONE-20261015`):
//!
//! * `CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS` (default ON since gci i1/og, `=0`
//!   kills): the old mark's BFS hands each popped object's young targets to
//!   `TrueRootYoung` in the scan it already makes, so `round` no longer
//!   rescans every marked old object;
//! * `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE` (default ON since gci i1/og,
//!   `=0` kills): the post-finalizer closure is skipped when the finalizer
//!   round marked nothing.
//!
//! Both must leave every mark exactly as the `=0` arm does: each heap-level
//! test runs the same fixture on both arms and compares.
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gch_h4pc_` (and `gci_i1og_`).

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

const IN_BFS: &str = "CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS";
const SKIP_POST: &str = "CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE";
/// gci i1/og: default ON since gci i1/og. The tests below pin it `=0` so the
/// watched set stays a seed (the d5/r rule they were written against); the ON
/// arm has its own test (`gci_i1og_...`).
const WATCHED_DEAD: &str = "CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD";

/// The flags a test pins: the two cuts as given (explicitly `1` or `0`: both
/// are default ON since gci i1/og and jr20 wave 9 lane G, so unset would read ON), the watched-dead
/// verdict off, the true-root seed on and wide (the defaults), and a
/// `System.gc()` that diverts to the non-moving cycle (so the requested major
/// is a true-root one).
fn pc_flags(in_bfs: bool, skip_post: bool) -> [(&'static str, Option<&'static str>); 7] {
    [
        (IN_BFS, Some(if in_bfs { "1" } else { "0" })),
        (SKIP_POST, Some(if skip_post { "1" } else { "0" })),
        (WATCHED_DEAD, Some("0")),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS", None),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE", None),
        ("CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG", None),
        // gci i8/rv: default ON since gci-user-flip-20261013; `old_verdicts`
        // reads the in-place arm's addresses, so the requested major must not
        // slide survivors over the freed blocks.
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

/// What a true-root major left: per address, is it still allocated in old
/// gen; and the old generation's true-root counters.
fn old_verdicts(heap: &GenerationalHeap, addrs: &[ObjectRef]) -> (Vec<bool>, (u64, u64, u64, u64)) {
    let og = heap.old_gen.lock();
    let alive = addrs
        .iter()
        .map(|r| og.is_allocated_addr(r.as_ptr()))
        .collect();
    let s = og.sizing_stats();
    (
        alive,
        (
            s.true_root_majors,
            s.true_root_young_excluded,
            s.true_root_promotions,
            s.true_root_promotions_dead,
        ),
    )
}

/// The `gcd_d4n` nepotism shape plus a chain that crosses young twice from a
/// root: `keep` (old, the root) -> `y1` (young) -> `q` (old) -> `y2` (young)
/// -> `r` (old), and a dead old `o` -> young `y3` -> old `p`. Runs one
/// requested true-root major under the given arm; returns the verdicts on
/// `[keep, q, r, o, p]` and the counters.
fn chain_major(in_bfs: bool) -> (Vec<bool>, (u64, u64, u64, u64)) {
    let (heap, old) = tenured(5);
    let monitors = NoMonitors;
    let (keep, q, r, o, p) = (old[0], old[1], old[2], old[3], old[4]);
    let y1 = heap.alloc_object(ClassId::new(0), 2);
    let y2 = heap.alloc_object(ClassId::new(0), 2);
    let y3 = heap.alloc_object(ClassId::new(0), 2);
    for y in [y1, y2, y3] {
        assert!(!heap.is_in_old(y.as_ptr()), "precondition: young");
    }
    heap.set_field(keep, 0, Value::Object(Some(y1)));
    heap.set_field(y1, 0, Value::Object(Some(q)));
    heap.set_field(q, 0, Value::Object(Some(y2)));
    heap.set_field(y2, 0, Value::Object(Some(r)));
    heap.set_field(o, 0, Value::Object(Some(y3)));
    heap.set_field(y3, 0, Value::Object(Some(p)));
    let mut roots = vec![keep];
    cratonvm_types::flags::with_thread_overrides(&pc_flags(in_bfs, false), || {
        crate::gc_quiescence::request_major_gc();
        heap.collect_garbage(&stw(), &mut roots, &monitors);
    });
    old_verdicts(&heap, &[keep, q, r, o, p])
}

/// The fold changes no mark: what a root reaches through young twice is kept,
/// the dead holder's young child's old target is freed in the same major, and
/// the true-root census (`true_root_young_excluded`) is the same on both arms.
#[test]
fn gch_h4pc_rounds_in_bfs_marks_exactly_what_round_marks_across_two_young_hops() {
    let (off_alive, off_counts) = chain_major(false);
    let (on_alive, on_counts) = chain_major(true);
    assert_eq!(
        off_alive,
        vec![true, true, true, false, false],
        "default arm: keep/q/r kept, o/p freed (the gcd_d4n verdict)",
    );
    assert_eq!(
        on_alive, off_alive,
        "ROUNDS_IN_BFS marks the same old objects"
    );
    assert_eq!(
        off_counts.0, 1,
        "precondition: a true-root major ran: {off_counts:?}"
    );
    assert!(
        off_counts.1 >= 1,
        "precondition: y3 was left out: {off_counts:?}"
    );
    assert_eq!(on_counts, off_counts, "the same census on both arms");
}

/// The `gcd_d5r` shape under the fold: a WATCHED young survivor that only a
/// dead old holder names keeps its old referent on both arms (the watched set
/// is a seed, not a BFS pop, so the fold must not lose it).
#[test]
fn gch_h4pc_rounds_in_bfs_keeps_a_watched_young_survivors_old_referent() {
    let run = |in_bfs: bool| {
        let (heap, old) = tenured(3);
        let monitors = NoMonitors;
        let (o, p, keep) = (old[0], old[1], old[2]);
        let y = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(y, 0, Value::Object(Some(p)));
        heap.set_field(o, 0, Value::Object(Some(y)));
        let mut roots = vec![keep];
        crate::gc_quiescence::set_watched_referents(&[y.as_ptr() as usize]);
        cratonvm_types::flags::with_thread_overrides(&pc_flags(in_bfs, false), || {
            crate::gc_quiescence::request_major_gc();
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        });
        crate::gc_quiescence::set_watched_referents(&[]);
        old_verdicts(&heap, &[keep, o, p])
    };
    let off = run(false);
    let on = run(true);
    assert_eq!(
        off.0,
        vec![true, false, true],
        "default: o freed, the watched y keeps p"
    );
    assert_eq!(on, off, "the same marks and census under ROUNDS_IN_BFS");
}

/// The `gcd_d9a` promoting shape under the fold: `y` is promoted by the
/// requested major's young phase (promote-on-pressure), nothing live reaches
/// its destination, so the destination and `p` die in the same major on both
/// arms, and the promotion census matches.
#[test]
fn gch_h4pc_rounds_in_bfs_drops_the_same_dead_promotion() {
    let run = |in_bfs: bool| {
        let (heap, old) = tenured(3);
        let monitors = NoMonitors;
        let (o, p, keep) = (old[0], old[1], old[2]);
        let y = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(y, 0, Value::Object(Some(p)));
        heap.set_field(o, 0, Value::Object(Some(y)));
        let mut roots = vec![keep];
        cratonvm_types::flags::with_thread_overrides(&pc_flags(in_bfs, false), || {
            heap.force_promote_all.store(true, Ordering::Relaxed);
            crate::gc_quiescence::request_major_gc();
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        });
        old_verdicts(&heap, &[keep, o, p])
    };
    let off = run(false);
    let on = run(true);
    assert_eq!(off.0, vec![true, false, false], "default: o and p freed");
    let (_, (_, _, promotions, promotions_dead)) = &off;
    assert_eq!(
        (*promotions, *promotions_dead),
        (1, 1),
        "precondition: y promoted, its copy dead: {off:?}",
    );
    assert_eq!(on, off, "the same marks and census under ROUNDS_IN_BFS");
}

/// A legacy old-gen object of `slots` slots with a real header.
fn old_obj(og: &mut OldGen, slots: u32) -> *mut u8 {
    let p = og
        .alloc(HEADER_SIZE + SLOT_SIZE * slots as usize, 8)
        .expect("room in old gen");
    // SAFETY: a fresh, zeroed block sized for a header and `slots` slots.
    unsafe { (*(p as *mut ObjectHeader)).set_num_slots(slots) };
    p
}

/// Store `target` into legacy slot `slot` of `holder`.
fn set_slot(holder: *mut u8, slot: usize, target: usize) {
    // SAFETY: `holder` is a live legacy object with more than `slot` slots;
    // `target` is non-null and 8-aligned (never dereferenced by these tests
    // unless it is an old-gen object base).
    unsafe {
        std::ptr::write(
            holder.add(HEADER_SIZE + slot * SLOT_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(target as *mut u8))),
        );
    }
}

/// The split scan: an old holder naming an old target and a "young" word.
/// Unsplit, the old target is marked and pushed and the young word goes
/// nowhere (the old mark returns for it); split, the same old push AND the
/// young word collected. A corrupt kind tag is rejected unscanned (`false`),
/// which is what sends the object to `round` on the fold arm.
#[test]
fn gch_h4pc_the_split_scan_collects_young_words_and_marks_the_same_old_ones() {
    // A stand-in young range: a buffer outside old gen, never dereferenced.
    let young_buf = vec![0u64; 8];
    let lo = young_buf.as_ptr() as usize;
    let hi = lo + young_buf.len() * 8;
    let young_word = lo + 16;
    for split in [false, true] {
        let mut og = OldGen::new(64 * 1024);
        let holder = old_obj(&mut og, 2);
        let target = old_obj(&mut og, 2);
        set_slot(holder, 0, target as usize);
        set_slot(holder, 1, young_word);
        let walked = og.walk_objects_with_gaps().0;
        let mut worklist: Vec<*mut u8> = Vec::new();
        let mut young: Vec<usize> = Vec::new();
        let scanned = GenerationalHeap::scan_object_for_old_refs_split_in(
            holder,
            &og,
            walked.as_slice(),
            &mut worklist,
            split.then_some((lo, hi, &mut young)),
        );
        assert!(scanned, "split={split}: a sane header is scanned");
        assert_eq!(
            worklist,
            vec![target],
            "split={split}: the old target is pushed"
        );
        // SAFETY: `target` is a live old-gen object base.
        let marked = unsafe { &*(target as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED != 0;
        assert!(marked, "split={split}: and marked");
        let want: Vec<usize> = if split { vec![young_word] } else { Vec::new() };
        assert_eq!(
            young, want,
            "split={split}: the young word is collected only when split"
        );
    }
    let mut og = OldGen::new(64 * 1024);
    let holder = old_obj(&mut og, 2);
    set_slot(holder, 1, young_word);
    // SAFETY: the `kind` byte of a live allocation; 0xFF is no ObjectKind.
    unsafe { std::ptr::write(holder.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8) };
    let mut worklist: Vec<*mut u8> = Vec::new();
    let mut young: Vec<usize> = Vec::new();
    let no_walk: &[(*mut u8, usize)] = &[];
    let scanned = GenerationalHeap::scan_object_for_old_refs_split_in(
        holder,
        &og,
        no_walk,
        &mut worklist,
        Some((lo, hi, &mut young)),
    );
    assert!(!scanned, "a corrupt kind tag is rejected unscanned");
    assert!(
        worklist.is_empty() && young.is_empty(),
        "and yields nothing"
    );
}

/// `TrueRootYoung::seed_from_bfs` seeds exactly what `seed_word` would for
/// the same values (interior and base of one grid object once; a promoted
/// source resolved to its promotion; free space and old words nothing), and
/// leaves `pending_old` empty, so `round` has nothing to rescan.
#[test]
fn gch_h4pc_seed_from_bfs_seeds_what_seed_word_seeds_and_queues_nothing_for_round() {
    let promoted = vec![(0x2000usize, 0x9000usize, 0x20usize)];
    let promoted_by_dest: FxHashMap<usize, usize> = [(0x9000usize, 0usize)].into_iter().collect();
    let mut y = TrueRootYoung {
        grid: vec![(0x1000, 0x20), (0x1020, 0x20), (0x1040, 0x20)],
        stretches: Vec::new(),
        lo: 0x1000,
        hi: 0x3000,
        loaders: None,
        mirror_rows: None, // gci i8/pa
        visited: TrueRootVisited::default(),
        pending_young: Vec::new(),
        pending_old: Vec::new(),
        promoted,
        promoted_by_dest,
        promoted_seen: vec![false],
        pending_promoted: Vec::new(),
    };
    let og = OldGen::new(4096);
    let walked: Vec<(*mut u8, usize)> = Vec::new();
    let owners: FxHashSet<usize> = FxHashSet::default();
    let ctx = TrueRootCtx {
        old_gen: &og,
        walked: &walked,
        mirror_pin_owners: &owners,
        metadata_pins: None,
    };
    let mut worklist: Vec<*mut u8> = Vec::new();
    // `obj` is the promotion's destination: its source-keyed rows are asked
    // (none here: no mirror or metadata pins), which must not disturb the seeds.
    y.seed_from_bfs(
        0x9000,
        &[0x1008, 0x1000, 0x2010, 0x1080],
        &ctx,
        &mut worklist,
    );
    assert_eq!(y.pending_young, vec![0x1000], "one grid seed, once");
    assert_eq!(
        y.pending_promoted,
        vec![0],
        "the promoted source resolved once"
    );
    assert!(
        y.pending_old.is_empty(),
        "nothing is queued for round's rescan"
    );
    assert!(
        worklist.is_empty(),
        "no source-keyed row, so nothing followed"
    );
    assert_eq!(y.excluded(), 2, "the two objects nothing named stay out");
}

/// The post-close skip's decision table: only an armed switch, inside the
/// finalizer round, with the post-close not yet done, nothing popped in the
/// round and a pre-close verdict to reuse.
#[test]
fn gch_h4pc_idle_post_close_verdict_table() {
    // (armed, draining, done, pops, pre) -> verdict
    let rows: [(bool, bool, bool, usize, Option<bool>, Option<bool>); 8] = [
        (true, true, false, 0, Some(false), Some(false)),
        (true, true, false, 0, Some(true), Some(true)),
        (false, true, false, 0, Some(false), None),
        (true, false, false, 0, Some(false), None),
        (true, true, true, 0, Some(false), None),
        (true, true, false, 1, Some(false), None),
        (true, true, false, 0, None, None),
        (false, false, true, 3, None, None),
    ];
    for (armed, draining, done, pops, pre, want) in rows {
        assert_eq!(
            idle_post_close_verdict(armed, draining, done, pops, pre),
            want,
            "armed={armed} draining={draining} done={done} pops={pops} pre={pre:?}",
        );
    }
}

/// The post-close skip changes no mark. A true-root sweep with finalizer
/// candidates, on both arms of both cuts: with only a LIVE candidate the
/// finalizer round is idle (the skip's case) and the garbage is freed alike;
/// with a DEAD candidate too, it is resurrected with its child, the depth-2
/// set names both, and the garbage is freed alike.
#[test]
fn gch_h4pc_skip_idle_post_close_keeps_the_same_marks_with_live_and_dead_candidates() {
    // -> (alive [live, dead, child, garbage], resurrected, closure sorted),
    // the last two as INDICES into that fixture, not addresses: every run
    // builds a fresh heap, so the same object sits at a different address
    // whenever a test running in parallel took the memory the previous run's
    // heap released (the flake this test had: identical marks, compared as
    // addresses of two different heaps).
    let run = |in_bfs: bool, skip: bool, with_dead: bool| {
        let (heap, old) = tenured(4);
        let (live, dead, child, garbage) = (old[0], old[1], old[2], old[3]);
        heap.set_field(dead, 0, Value::Object(Some(child)));
        let cands: Vec<usize> = if with_dead {
            vec![live.as_ptr() as usize, dead.as_ptr() as usize]
        } else {
            vec![live.as_ptr() as usize]
        };
        let mut pass = OldGenFinalizerPass::new(&cands);
        cratonvm_types::flags::with_thread_overrides(&pc_flags(in_bfs, skip), || {
            let _armed = TrueRootMajorScope::arm(true);
            let _ = heap.sweep_old_gen_non_moving_finalizing(
                &[live],
                &cratonvm_types::PointerMap::default(),
                &mut pass,
            );
        });
        let (alive, counts) = old_verdicts(&heap, &[live, dead, child, garbage]);
        assert_eq!(
            counts.0, 1,
            "precondition: a true-root major ran: {counts:?}"
        );
        let fixture = [live, dead, child, garbage].map(|o| o.as_ptr() as usize);
        let index = |a: &usize| {
            fixture
                .iter()
                .position(|f| f == a)
                .unwrap_or_else(|| panic!("0x{a:x} is not a fixture object: {fixture:x?}"))
        };
        let resurrected: Vec<usize> = pass.resurrected.iter().map(index).collect();
        let mut closure: Vec<usize> = pass.closure.iter().map(index).collect();
        closure.sort_unstable();
        (alive, resurrected, closure)
    };
    for with_dead in [false, true] {
        let base = run(false, false, with_dead);
        if with_dead {
            assert_eq!(
                base.0,
                vec![true, true, true, false],
                "dead candidate kept with its child"
            );
            assert_eq!(base.1.len(), 1, "one resurrected: {base:?}");
        } else {
            assert_eq!(
                base.0,
                vec![true, false, false, false],
                "idle round: all garbage freed"
            );
            assert!(
                base.1.is_empty() && base.2.is_empty(),
                "nothing resurrected: {base:?}"
            );
        }
        for (in_bfs, skip) in [(false, true), (true, false), (true, true)] {
            assert_eq!(
                run(in_bfs, skip, with_dead),
                base,
                "with_dead={with_dead} in_bfs={in_bfs} skip={skip}: the same marks",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// gci i1/og (2026-10-11): the three true-root switches flipped default ON
// (`CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS`, `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE`,
// `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`), and the `round` screen
// (`gch-h4pc-old-mark-and-true-root-review-findings-RETIRED-UNFIXED-20261018` item 2).
// Run: `cargo test -j 5 -p cratonvm-gc --lib gci_i1og_`.
// ---------------------------------------------------------------------------

/// Unset reads ON for all three; `0` / `false` kill each one; `1` keeps it
/// on.
#[test]
fn gci_i1og_the_three_true_root_switches_are_default_on_and_zero_kills_them() {
    let read = || {
        (
            true_root_rounds_in_bfs_enabled(),
            true_root_skip_idle_post_close_enabled(),
            full_gc_true_roots_watched_dead_enabled(),
        )
    };
    let unset = cratonvm_types::flags::with_thread_overrides(
        &[(IN_BFS, None), (SKIP_POST, None), (WATCHED_DEAD, None)],
        read,
    );
    assert_eq!(unset, (true, true, true), "default ON");
    for off in ["0", "false"] {
        let killed = cratonvm_types::flags::with_thread_overrides(
            &[(IN_BFS, Some(off)), (SKIP_POST, Some(off)), (WATCHED_DEAD, Some(off))],
            read,
        );
        assert_eq!(killed, (false, false, false), "`{off}` kills");
    }
    let on = cratonvm_types::flags::with_thread_overrides(
        &[(IN_BFS, Some("1")), (SKIP_POST, Some("1")), (WATCHED_DEAD, Some("1"))],
        read,
    );
    assert_eq!(on, (true, true, true), "`1` keeps each on");
    // One at a time: each kill is independent of the other two.
    for (i, name) in [IN_BFS, SKIP_POST, WATCHED_DEAD].into_iter().enumerate() {
        let got = cratonvm_types::flags::with_thread_overrides(&[(name, Some("0"))], read);
        let want = (i != 0, i != 1, i != 2);
        assert_eq!(got, want, "{name}=0 kills only itself");
    }
}

/// The `gcd_d5r` shape on the DEFAULT arm (watched-dead ON), on both round
/// arms: the watched young `y` only the dead old `o` names is not a seed, the
/// trace leaves it out, so it is judged dead (the verdict is installed) and
/// its old referent `p` is freed in the same major, as HotSpot's full
/// collection does. Both round arms mark the same.
#[test]
fn gci_i1og_watched_dead_frees_a_watched_young_survivors_referent_on_both_round_arms() {
    let run = |in_bfs: bool| {
        let (heap, old) = tenured(3);
        let monitors = NoMonitors;
        let (o, p, keep) = (old[0], old[1], old[2]);
        let y = heap.alloc_object(ClassId::new(0), 2);
        assert!(!heap.is_in_old(y.as_ptr()), "precondition: y is young");
        heap.set_field(y, 0, Value::Object(Some(p)));
        heap.set_field(o, 0, Value::Object(Some(y)));
        let y_addr = y.as_ptr() as usize;
        let mut roots = vec![keep];
        let mut flags = pc_flags(in_bfs, false).to_vec();
        // Unset after the `=0` above: the default (ON).
        flags.push((WATCHED_DEAD, None));
        crate::gc_quiescence::set_watched_referents(&[y_addr]);
        let result = cratonvm_types::flags::with_thread_overrides(&flags, || {
            crate::gc_quiescence::request_major_gc();
            heap.collect_garbage(&stw(), &mut roots, &monitors)
        });
        crate::gc_quiescence::set_watched_referents(&[]);
        let judged = heap.true_root_watched_left_out(y_addr);
        let in_map = result.pointer_map.contains_key(&y_addr);
        (old_verdicts(&heap, &[keep, o, p]), judged, in_map)
    };
    let off = run(false);
    let on = run(true);
    assert_eq!(off.0 .0, vec![true, false, false], "keep kept; o and p freed");
    assert!(off.1, "the watched young survivor is judged dead");
    assert!(!off.2, "and leaves the returned map");
    assert_eq!(off.0 .1 .0, 1, "precondition: one true-root major: {off:?}");
    assert_eq!(on, off, "the same marks, verdict and census under ROUNDS_IN_BFS");
}

/// gch h4pc review item 2: `TrueRootYoung::round` now screens a pending
/// object as the BFS's scan does before it walks its slots. A sane object
/// passes; a bad kind tag and an extent that leaves old gen are refused.
#[test]
fn gci_i1og_round_screens_an_object_the_bfs_would_reject() {
    let mut og = OldGen::new(64 * 1024);
    let sane = old_obj(&mut og, 2);
    assert!(old_object_slots_scannable(sane, &og), "a sane header is scannable");
    let bad_kind = old_obj(&mut og, 2);
    // SAFETY: the `kind` byte of a live allocation; 0xFF is no ObjectKind.
    unsafe { std::ptr::write(bad_kind.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8) };
    assert!(!old_object_slots_scannable(bad_kind, &og), "a corrupt kind tag is refused");
    let huge = old_obj(&mut og, 2);
    // SAFETY: a live legacy object's header; the slot count now claims an
    // extent far past the end of this 64 KiB generation.
    unsafe { (*(huge as *mut ObjectHeader)).set_num_slots(1 << 20) };
    assert!(!old_object_slots_scannable(huge, &og), "an extent leaving old gen is refused");
}

/// gch h4pc review item 3: only the moving cycle's ask (P-C) is marked
/// pre-copy, so only there does `old_gen_gc_inner` skip the watched seed and
/// the watched-dead verdict (the moving watched shape itself is
/// `gch_h2og_a_watched_young_survivor_keeps_its_old_referent_through_a_moving_true_root_major`,
/// which now runs with the verdict's switch at its default, ON).
#[test]
fn gci_i1og_only_the_moving_cycles_ask_is_pre_copy() {
    let ask = moving_true_root_ask(7);
    assert!(ask.pre_copy, "the moving ask holds pre-copy watched addresses");
    assert_eq!(ask.promo_tail, 7);
    assert!(ask.promotions.is_empty(), "no promotion is resolved through a source");
    // gci i3/og: the moving cycle fills these itself (PWD's copies, the
    // carried watched snapshot); the bare ask carries neither.
    assert!(ask.watched_post_copy.is_empty());
    assert!(ask.watched_snapshot.is_none());
    // gci i4/og: P-C judges from its `pc_tail`; no judged list is handed back.
    assert!(!ask.hand_back_judged);
}

/// gci i2/og (the P-C cost cut): a trace whose reached set is the grid-index
/// bitset (`TrueRootVisited::ByIndex`, the P-C ask's) seeds the same objects
/// once each, excludes the same count and judges the same watched addresses
/// as one with the address set (`ByAddr`, every non-moving major's). Words:
/// interior, base, a repeat, an object's last byte, a gap after an object,
/// one past the last object, and outside young.
#[test]
fn gci_i2og_index_visited_set_answers_as_the_address_set() {
    let grid: Vec<(usize, usize)> =
        vec![(0x1000, 0x20), (0x1020, 0x20), (0x1040, 0x20), (0x1080, 0x40)];
    let mk = |visited: TrueRootVisited| TrueRootYoung {
        grid: grid.clone(),
        stretches: Vec::new(),
        lo: 0x1000,
        hi: 0x3000,
        loaders: None,
        mirror_rows: None, // gci i8/pa
        visited,
        pending_young: Vec::new(),
        pending_old: Vec::new(),
        promoted: Vec::new(),
        promoted_by_dest: FxHashMap::default(),
        promoted_seen: Vec::new(),
        pending_promoted: Vec::new(),
    };
    let mut by_addr = mk(TrueRootVisited::default());
    let mut by_index = mk(TrueRootVisited::by_index(grid.len()));
    for w in [0x1008usize, 0x1000, 0x1088, 0x10bf, 0x1060, 0x10c0, 0x8000_0000, 0x1088] {
        by_addr.seed_word(w);
        by_index.seed_word(w);
    }
    assert_eq!(by_index.pending_young, vec![0x1000, 0x1080], "each reached object once");
    assert_eq!(by_index.pending_young, by_addr.pending_young);
    assert_eq!(by_index.excluded(), 2);
    assert_eq!(by_index.excluded(), by_addr.excluded());
    // gci i21/ws: the sweeps' watched set is a `WatchedSnapshot` now.
    let watched =
        crate::gc_quiescence::WatchedSnapshot::from_addrs(&[0x1010, 0x1028, 0x1048, 0x1090, 0x2fff]);
    let left_out = watched_left_out_by_trace(&by_index, &watched);
    assert_eq!(left_out, vec![0x1028, 0x1048], "the two unreached objects' addresses");
    assert_eq!(left_out, watched_left_out_by_trace(&by_addr, &watched));
    // A bitset sized short still records (it grows) and answers an index it
    // never covered as not reached.
    let mut v = TrueRootVisited::by_index(0);
    assert!(v.insert(130, 0xdead_0000));
    assert!(!v.insert(130, 0xdead_0000), "a second insert is not new");
    assert!(v.contains(130, 0) && !v.contains(131, 0) && !v.contains(10_000, 0));
    assert_eq!(v.len(), 1);
}
