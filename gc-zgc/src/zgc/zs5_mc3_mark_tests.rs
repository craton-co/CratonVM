//! zs s5/mc3: stealable reference-array chunks over the REAL heap
//! (`CRATONVM_ZGC_MARK_SPLIT_ARRAYS`).
use super::*;
use mark::ZMarkContext;

/// A reference array of `9 x chunk + 5` elements (every third null, the
/// rest small `long[]` leaves), a second one nested in it, and garbage.
fn arrays(heap: &ZgcRealHeap, chunk: usize) -> (ObjectRef, Vec<ObjectRef>, ObjectRef) {
    let mut live = Vec::new();
    let outer_len = 9 * chunk + 5;
    let outer = heap.alloc_array(ClassId::new(1), ArrayElementType::Reference, outer_len);
    live.push(outer);
    let inner_len = 3 * chunk + 1;
    let inner = heap.alloc_array(ClassId::new(1), ArrayElementType::Reference, inner_len);
    live.push(inner);
    for (arr, len) in [(outer, outer_len), (inner, inner_len)] {
        for i in 0..len {
            if i % 3 == 2 {
                continue;
            }
            let e = heap.alloc_array(ClassId::new(1), ArrayElementType::Long, 1);
            live.push(e);
            heap.set_array_element(arr, i, Value::Object(Some(e)))
                .unwrap();
        }
    }
    // A slot the loop left null (578 % 3 == 2), in the final chunk.
    assert_eq!((outer_len - 3) % 3, 2);
    heap.set_array_element(outer, outer_len - 3, Value::Object(Some(inner)))
        .unwrap();
    let garbage = heap.alloc_array(ClassId::new(1), ArrayElementType::Long, 1);
    (outer, live, garbage)
}

/// The contract `ref_array_len` promises, on the real heap: `Some(len)`
/// for a reference array only, and the chunked walk visits exactly
/// `[cursor, cursor + budget)` with `None` only on the final chunk.
#[test]
fn zs5mc3_the_real_heap_offers_reference_arrays_only() {
    let heap = ZgcRealHeap::with_capacity(4 * 1024 * 1024);
    heap.set_tlab_enabled(false);
    let refs = heap.alloc_array(ClassId::new(1), ArrayElementType::Reference, 10);
    let longs = heap.alloc_array(ClassId::new(1), ArrayElementType::Long, 10);
    let obj = heap.alloc_object(ClassId::new(1), 2);
    assert_eq!(heap.ref_array_len(refs.as_ptr() as u64), Some(10));
    assert_eq!(heap.ref_array_len(longs.as_ptr() as u64), None);
    assert_eq!(heap.ref_array_len(obj.as_ptr() as u64), None);
    assert_eq!(heap.ref_array_len(0), None);
    for i in 0..10 {
        let e = heap.alloc_array(ClassId::new(1), ArrayElementType::Long, 1);
        heap.set_array_element(refs, i, Value::Object(Some(e)))
            .unwrap();
    }
    let _skip = heap.begin_concurrent_mark_cycle();
    let mut scratch = mark::ZMarkScratch::default();
    let mut local = mark::ZMarkLocalStack::new();
    let a = refs.as_ptr() as u64;
    let _ = heap.mark_clear_all();
    let mut sink = mark::ZMarkEdgeSink::new(&mut local);
    assert_eq!(
        heap.scan_and_claim(a, 3, 4, &mut scratch, &mut sink),
        Some(7)
    );
    assert_eq!(sink.marked(), 4, "elements 3..7 only");
    let mut sink = mark::ZMarkEdgeSink::new(&mut local);
    assert_eq!(heap.scan_and_claim(a, 7, 4, &mut scratch, &mut sink), None);
    assert_eq!(sink.marked(), 3, "elements 7..10, the final chunk");
    heap.end_concurrent_mark_cycle();
}

/// The whole engine on the real heap, split arm vs kill arm on one pool:
/// the same mark set, the garbage unmarked, nothing off-heap; the split
/// arm splits both arrays and runs every task it pushed (positive
/// control), the kill arm splits nothing and parks chunks instead.
#[test]
fn zs5mc3_the_real_heap_marks_the_same_set_on_both_arms() {
    let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
    heap.set_tlab_enabled(false);
    let chunk = 64usize;
    let (outer, live, garbage) = arrays(&heap, chunk);
    let root = outer.as_ptr() as u64;
    for split in [true, false, true] {
        let pool = heap
            .persistent_mark_pool(4)
            .expect("the persistent pool spawns");
        pool.set_split_arrays(split);
        pool.set_ref_chunk(chunk);
        drop(pool);
        let _ = heap.mark_clear_all();
        let _skip = heap.begin_concurrent_mark_cycle();
        let stats = heap
            .mark_parallel_stw(&[root], 4)
            .expect("a safepoint mark certifies");
        heap.end_concurrent_mark_cycle();
        let missed: Vec<usize> = live
            .iter()
            .enumerate()
            .filter(|(_, o)| !heap.mark_is_set(o.as_ptr() as usize))
            .map(|(i, _)| i)
            .collect();
        assert!(
            missed.is_empty(),
            "split={split}: every live object; missed {} (first {:?}) of {}: {stats:?}",
            missed.len(),
            &missed[..missed.len().min(8)],
            live.len()
        );
        assert!(
            !heap.mark_is_set(garbage.as_ptr() as usize),
            "split={split}"
        );
        assert_eq!(stats.off_heap_children, 0, "split={split}");
        assert_eq!(
            stats.objects_marked + stats.roots_marked,
            live.len() as u64,
            "split={split}: each object claimed once: {stats:?}"
        );
        if split {
            // 9 x 64 + 5 -> 10 chunks (9 tasks); 3 x 64 + 1 -> 4 (3).
            assert_eq!(stats.array_splits, 2, "{stats:?}");
            assert_eq!(stats.array_tasks, 9 + 3, "{stats:?}");
            assert_eq!(stats.array_task_runs, 9 + 3, "{stats:?}");
        } else {
            assert_eq!(
                (stats.array_splits, stats.array_tasks),
                (0, 0),
                "kill arm: {stats:?}"
            );
            assert!(stats.ref_chunks > 0, "kill arm: {stats:?}");
        }
    }
}
