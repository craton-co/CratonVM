use super::*;

fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: these tests run the heap single-threaded; no mutator runs.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    })
}

/// Finding 1: a holder carved AFTER a frozen tail but BEFORE the initial
/// mark sits above the lowered TAMS (implicitly live) although it predates
/// the snapshot; the object it names sits below TAMS. With no roots, the
/// i16 start left the holder LIVE and its target DEAD (cleanup would free
/// the target under a holder a weak reference still hands out). The start
/// now grays the holder, so the drained mark keeps the target.
#[test]
fn gci_i17rv_a_pre_snapshot_holder_above_a_lowered_tams_is_traced() {
    let gc = collector();
    let target = gc.alloc_object(ClassId::new(9), 1);
    gc.set_field(target, 0, Value::Int(5));
    let (chunk, len) = gc.refill_tlab(64 * 1024).expect("a TLAB chunk");
    let tail = chunk as usize;
    let holder = gc.alloc_object(ClassId::new(9), 1);
    gc.set_field(holder, 0, Value::Object(Some(target)));
    let (t, h) = (target.as_ptr() as usize, holder.as_ptr() as usize);
    let region = gc.lookup_region_for_addr(tail).expect("the chunk is in a region");
    assert!(
        gc.lookup_region_for_addr(t) == Some(region)
            && gc.lookup_region_for_addr(h) == Some(region)
            && t < tail
            && h >= tail + len,
        "fixture: target below the tail, holder above it, one region \
         (t={t:#x} tail={tail:#x}+{len:#x} h={h:#x})"
    );

    gc.set_jit_tlab_skip_regions(&[(tail, tail + len)]);
    gc.start_concurrent_mark(&stw());
    while !gc.concurrent_mark_step(usize::MAX) {}
    let holder_live = gc.is_live_after_mark(h);
    let target_live = gc.is_live_after_mark(t);
    let target_marked = {
        let regions = gc.regions.read();
        regions[region].mark_bitmap.is_marked(t)
    };
    gc.abort_concurrent_mark();
    gc.clear_jit_tlab_skip_regions();

    assert!(holder_live, "above the lowered TAMS the holder is implicitly live");
    assert!(target_marked, "the start traced the implicitly-live holder");
    assert!(target_live, "what an implicitly-live object names is live");
}

/// Finding 1, the walk itself: tails are skipped, every object start
/// outside them is reported, and nothing below `from` is.
#[test]
fn gci_i17rv_the_lowered_tams_walk_reports_the_starts_outside_the_tails() {
    let gc = collector();
    let below = gc.alloc_object(ClassId::new(9), 1);
    let (chunk, len) = gc.refill_tlab(32 * 1024).expect("a TLAB chunk");
    let tail = chunk as usize;
    let a = gc.alloc_object(ClassId::new(9), 2);
    let b = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 4);
    let region = gc.lookup_region_for_addr(tail).expect("the chunk is in a region");
    let (base, cursor) = {
        let regions = gc.regions.read();
        (regions[region].data.addr(), regions[region].cursor())
    };
    let mut out = Vec::new();
    // SAFETY: single-threaded test heap; `[tail - base, cursor)` is inside
    // the region's allocated prefix.
    let complete = unsafe {
        pre_snapshot_starts_above_lowered_tams(
            base,
            tail - base,
            cursor,
            &[(tail, tail + len)],
            &mut out,
        )
    };
    assert!(complete, "the walk reached the cursor");
    assert!(out.contains(&(a.as_ptr() as usize)));
    assert!(out.contains(&(b.as_ptr() as usize)));
    assert!(!out.contains(&(below.as_ptr() as usize)), "nothing below the tail");
    assert!(
        out.iter().all(|&x| !(tail..tail + len).contains(&x)),
        "nothing inside the tail"
    );
}
