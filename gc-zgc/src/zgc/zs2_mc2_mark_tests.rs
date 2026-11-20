//! zs s2/mc2: the engine's direct scan (`CRATONVM_ZGC_MARK_DIRECT_SCAN`)
//! and its per-worker oop-map memo.
use super::*;
use cratonvm_types::FieldStorageKind;
use mark::ZMarkContext;

/// Two refs (fields 0 and 2) and an int.
const NODE: u32 = 5201;
/// One ref (field 0).
const LINK: u32 = 5202;
/// Registered only to bump the layout generation.
const BUMP: u32 = 5203;

fn register(heap: &ZgcRealHeap, class: u32, refs: &[bool]) {
    let field_disps: Vec<u32> = (0..refs.len() as u32).map(|i| 8 + 8 * i).collect();
    let ref_disps: Vec<u32> = field_disps
        .iter()
        .zip(refs)
        .filter(|(_, r)| **r)
        .map(|(d, _)| *d)
        .collect();
    cratonvm_types::register_class_layout(
        heap.layout_domain(),
        class,
        std::sync::Arc::new(cratonvm_types::CompactLayout {
            field_disps,
            is_ref: refs.to_vec(),
            field_kinds: refs
                .iter()
                .map(|r| {
                    if *r {
                        FieldStorageKind::Reference
                    } else {
                        FieldStorageKind::Int
                    }
                })
                .collect(),
            ref_disps,
            total_size: 8 + 8 * refs.len() as u32,
        }),
    );
}

struct Graph {
    heap: ZgcRealHeap,
    root: ObjectRef,
    /// Every object that must be marked from `root`; `live[1]` is the
    /// tree root, the last two are the weak `Reference` and its strong
    /// field's target.
    live: Vec<ObjectRef>,
    /// Reachable only through the weak `Reference`'s referent slot.
    referent: ObjectRef,
    garbage: ObjectRef,
}

fn tree(heap: &ZgcRealHeap, live: &mut Vec<ObjectRef>, depth: u32, k: &mut u32) -> ObjectRef {
    let n = heap.alloc_object(ClassId::new(NODE), 3);
    live.push(n);
    if depth == 0 {
        *k += 1;
        if *k % 2 == 0 {
            let l = heap.alloc_object(ClassId::new(LINK), 1);
            let p = heap.alloc_array(ClassId::new(1), ArrayElementType::Int, 8);
            heap.set_field(l, 0, Value::Object(Some(p)));
            heap.set_field(n, 0, Value::Object(Some(l)));
            live.push(l);
            live.push(p);
        }
        return n;
    }
    let l = tree(heap, live, depth - 1, k);
    let r = tree(heap, live, depth - 1, k);
    heap.set_field(n, 0, Value::Object(Some(l)));
    heap.set_field(n, 2, Value::Object(Some(r)));
    n
}

/// Compact NODE tree (depth 5) whose leaves alternate between a LINK ->
/// int[] chain and nothing (memo hits and refills), a reference array
/// longer than two chunks holding nodes, primitive arrays and nulls, a
/// legacy root, and a compact weak `Reference` whose referent is
/// otherwise unreachable.
fn graph() -> Graph {
    let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
    heap.set_tlab_enabled(false);
    register(&heap, NODE, &[true, false, true]);
    register(&heap, LINK, &[true]);
    let mut live = Vec::new();
    let mut k = 0;
    let root = heap.alloc_object(ClassId::new(1), 4); // legacy cells
    live.push(root);
    let t = tree(&heap, &mut live, 5, &mut k);
    let chunk = mark::ref_chunk_budget();
    let arr = heap.alloc_array(ClassId::new(1), ArrayElementType::Reference, 2 * chunk + 3);
    live.push(arr);
    for i in (0..2 * chunk + 3).step_by(3) {
        let e = if i % 2 == 0 {
            heap.alloc_object(ClassId::new(NODE), 3)
        } else {
            heap.alloc_array(ClassId::new(1), ArrayElementType::Long, 2)
        };
        live.push(e);
        heap.set_array_element(arr, i, Value::Object(Some(e)))
            .unwrap();
    }
    let weak = heap.alloc_object(ClassId::new(NODE), 3);
    live.push(weak);
    let referent = heap.alloc_object(ClassId::new(NODE), 3);
    heap.set_field(weak, 0, Value::Object(Some(referent)));
    // Field 2 of the Reference is an ordinary strong edge.
    let strong = heap.alloc_object(ClassId::new(LINK), 1);
    live.push(strong);
    heap.set_field(weak, 2, Value::Object(Some(strong)));
    heap.discover_reference(ReferenceType::Weak, weak, referent, None);
    heap.set_field(root, 0, Value::Object(Some(t)));
    heap.set_field(root, 1, Value::Object(Some(arr)));
    heap.set_field(root, 2, Value::Object(Some(weak)));
    let garbage = heap.alloc_object(ClassId::new(NODE), 3);
    let header = unsafe { &*(t.as_ptr() as *const ObjectHeader) };
    assert!(
        cratonvm_types::is_compact_object(header),
        "the tree must be compact, or the memo arm is vacuous"
    );
    Graph {
        heap,
        root,
        live,
        referent,
        garbage,
    }
}

/// Per object, the closure path's claimed-child sequence equals the
/// direct scan's local stack, chunk by chunk, with the referent hole
/// open (a cycle's skip set) -- the pushes, not only the set.
#[test]
fn zs2mc2_the_direct_scan_pushes_the_closure_paths_sequence() {
    let g = graph();
    let heap = &g.heap;
    let _skip = heap.begin_concurrent_mark_cycle();
    let budget = mark::ref_chunk_budget();
    let mut scratch = mark::ZMarkScratch::default();
    let weak = g.live[g.live.len() - 2].as_ptr() as u64;
    let strong = g.live[g.live.len() - 1].as_ptr() as u64;
    let mut hole_checked = false;
    for o in g.live.iter().chain([&g.referent, &g.garbage]) {
        let addr = o.as_ptr() as u64;
        // Closure path, as the drain's `else` arm.
        let _ = heap.mark_clear_all();
        let mut expected: Vec<u64> = Vec::new();
        let mut off_expected = 0u64;
        let mut cursor = 0usize;
        loop {
            let next = heap.visit_refs_chunked(addr, cursor, budget, &mut |c| {
                if c == 0 {
                    return;
                }
                match heap.claim_child(c) {
                    mark::ZChildClaim::OffHeap => off_expected += 1,
                    mark::ZChildClaim::AlreadyMarked => {}
                    mark::ZChildClaim::Claimed => expected.push(c),
                }
            });
            match next {
                Some(n) => cursor = n,
                None => break,
            }
        }
        // Direct path.
        let _ = heap.mark_clear_all();
        let mut local = mark::ZMarkLocalStack::new();
        let (mut marked, mut off) = (0u64, 0u64);
        let mut cursor = 0usize;
        loop {
            let mut sink = mark::ZMarkEdgeSink::new(&mut local);
            let next = heap.scan_and_claim(addr, cursor, budget, &mut scratch, &mut sink);
            marked += sink.marked();
            off += sink.off_heap();
            match next {
                Some(n) => cursor = n,
                None => break,
            }
        }
        let mut got = Vec::new();
        while let Some(a) = local.pop() {
            got.push(a);
        }
        got.reverse();
        assert_eq!(got, expected, "object {addr:#x}: same pushes, same order");
        assert_eq!(marked, expected.len() as u64, "object {addr:#x}");
        assert_eq!((off, off_expected), (0, 0), "object {addr:#x}");
        if addr == weak {
            // The weak Reference: its strong field only, never the referent.
            assert_eq!(got, vec![strong], "the referent hole");
            hole_checked = true;
        }
    }
    heap.end_concurrent_mark_cycle();
    assert!(hole_checked, "the Reference row ran");
    assert!(
        scratch.memo_hits > 0 && scratch.memo_misses >= 2,
        "the memo engaged and refilled across classes: {scratch:?}"
    );
}

/// The whole engine, both arms of the switch on one pool: same mark set,
/// the referent hole honoured, nothing off-heap; the memo counters are
/// the POSITIVE CONTROL (non-zero only on the direct arm).
#[test]
fn zs2mc2_the_engine_marks_the_same_set_on_both_arms() {
    let g = graph();
    let heap = &g.heap;
    let mut baseline: Option<Vec<bool>> = None;
    for on in [true, false, true] {
        let pool = heap
            .persistent_mark_pool(2)
            .expect("the persistent pool spawns");
        pool.set_direct_scan(on);
        let _ = heap.mark_clear_all();
        let _skip = heap.begin_concurrent_mark_cycle();
        let stats = heap
            .mark_parallel_stw(&[g.root.as_ptr() as u64], 2)
            .expect("a safepoint mark certifies");
        heap.end_concurrent_mark_cycle();
        let marked: Vec<bool> = g
            .live
            .iter()
            .map(|o| heap.mark_is_set(o.as_ptr() as usize))
            .collect();
        assert!(marked.iter().all(|m| *m), "direct={on}: every live object");
        assert!(
            !heap.mark_is_set(g.referent.as_ptr() as usize),
            "direct={on}: the referent hole"
        );
        assert!(
            !heap.mark_is_set(g.garbage.as_ptr() as usize),
            "direct={on}"
        );
        assert_eq!(stats.off_heap_children, 0, "direct={on}");
        match &baseline {
            None => baseline = Some(marked),
            Some(b) => assert_eq!(&marked, b, "direct={on}"),
        }
        if on {
            assert!(stats.layout_memo_hits > 0, "direct arm: {stats:?}");
        } else {
            assert_eq!(
                (stats.layout_memo_hits, stats.layout_memo_misses),
                (0, 0),
                "kill arm: {stats:?}"
            );
        }
    }
}

/// A layout registration bumps the generation, and the memo refills on
/// the next object instead of answering from the old entry.
#[test]
fn zs2mc2_a_layout_registration_invalidates_the_memo() {
    let g = graph();
    let heap = &g.heap;
    let _skip = heap.begin_concurrent_mark_cycle();
    let mut scratch = mark::ZMarkScratch::default();
    let node = g.live[1].as_ptr() as u64; // the tree root, a NODE
    let scan = |scratch: &mut mark::ZMarkScratch| {
        let _ = heap.mark_clear_all();
        let mut local = mark::ZMarkLocalStack::new();
        let mut sink = mark::ZMarkEdgeSink::new(&mut local);
        assert_eq!(heap.scan_and_claim(node, 0, 4096, scratch, &mut sink), None);
        sink.marked()
    };
    let first = scan(&mut scratch);
    assert_eq!(first, 2, "a NODE with two children");
    assert_eq!(scratch.memo_misses, 1);
    // The generation is process-wide and other tests register layouts in
    // parallel, so wait for one undisturbed hit rather than assume it.
    let mut hit = false;
    for _ in 0..1000 {
        let h0 = scratch.memo_hits;
        assert_eq!(scan(&mut scratch), first);
        if scratch.memo_hits > h0 {
            hit = true;
            break;
        }
    }
    assert!(hit, "the memo serves a repeat of the same class");
    let misses = scratch.memo_misses;
    let hits = scratch.memo_hits;
    register(heap, BUMP, &[false]);
    assert_eq!(scan(&mut scratch), first, "same edges after the refill");
    assert_eq!(
        (scratch.memo_hits, scratch.memo_misses),
        (hits, misses + 1),
        "the generation changed, so the memo refilled"
    );
    heap.end_concurrent_mark_cycle();
}
/// zs s3/rv2: a PANICKING POOL WORKER makes the stop-the-world mark
/// uncertified (`None`, the serial fallback follows), and the pool is out
/// of the persistent slot with every worker JOINED before
/// `mark_parallel_stw` returns -- no straggler can claim beside the serial
/// marker. Before the fix the `!mark_set_complete` arm returned with the
/// stopped pool still in the slot and its workers unjoined (this test's
/// `Weak` still upgraded).
#[test]
fn zs3rv2_a_panicking_worker_leaves_no_running_pool_behind() {
    let g = graph();
    let heap = &g.heap;
    let pool = heap
        .persistent_mark_pool(2)
        .expect("the persistent pool spawns");
    let shared = std::sync::Arc::downgrade(pool.shared());
    drop(pool);
    // Every live object but the root: whichever a worker scans first.
    let mut armed = false;
    for victim in g.live.iter().skip(1) {
        heap.counters
            .test_worker_scan_panic_at
            .store(victim.as_ptr() as usize, Ordering::Relaxed);
        let _ = heap.mark_clear_all();
        let _skip = heap.begin_concurrent_mark_cycle();
        let got = heap.mark_parallel_stw(&[g.root.as_ptr() as u64], 2);
        heap.end_concurrent_mark_cycle();
        if got.is_none() {
            armed = true;
            break;
        }
    }
    heap.counters
        .test_worker_scan_panic_at
        .store(0, Ordering::Relaxed);
    assert!(
        armed,
        "positive control: a worker scanned a victim and panicked"
    );
    assert!(
        heap.counters.mark_pool.lock().is_none(),
        "the failed pool left the persistent slot"
    );
    assert!(
        shared.upgrade().is_none(),
        "every worker of the failed pool was joined before the return"
    );
}

/// zt t1/mk: the DRIVER-panic arm of `mark_with_controller_stw`, which no
/// test reached ("argued, not exercised" on the zs2mc2 page). The driver
/// panics at its first mark-end safepoint with the workers inside the
/// cycle; the arm must answer `None` (the serial fallback), take the pool
/// out of the persistent slot and join every worker before returning --
/// and (rv3 item 3) book the retired pool's CPU, which is a lower bound
/// and may read 0 on a coarse clock, so only its monotonicity is checked.
/// Control: the same call without the seam certifies.
#[test]
fn zt1mk_a_panicking_driver_leaves_no_running_pool_behind() {
    let g = graph();
    let heap = &g.heap;
    let _ = heap.mark_clear_all();
    let _skip = heap.begin_concurrent_mark_cycle();
    let ok = heap.mark_parallel_stw(&[g.root.as_ptr() as u64], 2);
    heap.end_concurrent_mark_cycle();
    assert!(ok.is_some(), "control: without the seam the pool certifies");

    let pool = heap
        .persistent_mark_pool(2)
        .expect("the persistent pool spawns");
    let shared = std::sync::Arc::downgrade(pool.shared());
    drop(pool);
    let cpu_before = heap.mark_cpu_totals().0;
    heap.counters
        .test_driver_safepoint_panic
        .store(true, Ordering::Relaxed);
    let _ = heap.mark_clear_all();
    let _skip = heap.begin_concurrent_mark_cycle();
    let got = heap.mark_parallel_stw(&[g.root.as_ptr() as u64], 2);
    heap.end_concurrent_mark_cycle();
    heap.counters
        .test_driver_safepoint_panic
        .store(false, Ordering::Relaxed);
    assert!(got.is_none(), "a panicking driver certifies nothing");
    assert!(
        heap.counters.mark_pool.lock().is_none(),
        "the failed pool left the persistent slot"
    );
    assert!(
        shared.upgrade().is_none(),
        "every worker of the failed pool was joined before the return"
    );
    // zt t3/mk (X1-13): `>=` alone could not fail (a cumulative counter).
    // The failed pool's workers drained the graph to a fixed point and
    // charged each busy stretch before they parked, so on Linux's exact
    // per-thread clock (`CLOCK_THREAD_CPUTIME_ID`) the panic arm's booking
    // is strictly positive; elsewhere the clock may be quantum-rounded
    // (Windows `GetThreadTimes`, ~15.6 ms) and 0 is a legal reading.
    let cpu_after = heap.mark_cpu_totals().0;
    if cfg!(target_os = "linux") && mark::mark_cpu_accounting_enabled() {
        assert!(
            cpu_after > cpu_before,
            "the driver-panic arm booked the retired pool's CPU ({cpu_before} -> {cpu_after})"
        );
    } else {
        assert!(cpu_after >= cpu_before);        }    }

/// zt t8/rl2 (residuals page item 6): the uncertified arm books only what
/// stragglers charged after the snapshot it already booked.
#[test]
fn zt8rl2_the_uncertified_arm_books_only_the_straggler_cpu() {
    assert_eq!(zgc_mark_straggler_cpu(10, 25), 15, "after the snapshot");
    assert_eq!(zgc_mark_straggler_cpu(25, 25), 0, "nothing after");
    assert_eq!(zgc_mark_straggler_cpu(25, 10), 0, "never negative");    }