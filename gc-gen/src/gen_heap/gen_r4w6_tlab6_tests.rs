use super::*;
use cratonvm_gc_core::tlab::Tlab;

const KEY_SINK: &str = "CRATONVM_GEN_TLAB_TAIL_SINK";

/// A 1 MiB / 1 MiB heap built with the sink switch `on`. The switch is
/// latched when the heap is built, so the override need not outlive this.
fn heap_with_sink(on: bool) -> GenerationalHeap {
    cratonvm_types::flags::with_thread_overrides(
        &[(KEY_SINK, Some(if on { "1" } else { "0" }))],
        || GenerationalHeap::with_sizes(1 << 20, 1 << 20),
    )
}

/// A TLAB over a fresh `size`-byte carve of `heap`, armed with the heap as
/// its tail sink exactly as the VM's refill site arms it.
fn armed_tlab(heap: &GenerationalHeap, size: usize) -> (Tlab, usize, usize) {
    let (ptr, got) = heap
        .refill_tlab(size)
        .expect("a fresh semi-space fits the carve");
    assert_eq!(got, size, "the whole request was carved");
    // SAFETY: `refill_tlab` handed this caller `[ptr, ptr + got)`, zeroed
    // and 8-aligned; nothing else can be handed it until the TLAB retires.
    let mut tlab = unsafe { Tlab::new(ptr, got) };
    if heap.tlab_tail_sink_enabled() {
        // SAFETY: `heap` outlives the TLAB in every caller below.
        unsafe { tlab.attach_tail_sink(heap) };
    }
    (tlab, ptr as usize, got)
}

fn young_used(heap: &GenerationalHeap) -> usize {
    heap.lock_young_from().used()
}

/// The flag is ON by default since gen r5w2 (its A/B) and latched per
/// heap; with `=0`, a retire never reaches the heap (the refill site does
/// not arm the buffer) and a direct call is refused without touching the
/// arena or the counters.
#[test]
fn the_tail_sink_is_on_by_default_and_refuses_when_off() {
    let default_heap =
        cratonvm_types::flags::with_thread_overrides(&[(KEY_SINK, None)], || {
            GenerationalHeap::with_sizes(1 << 20, 1 << 20)
        });
    assert!(
        default_heap.tlab_tail_sink_enabled(),
        "default ON since gen r5w2"
    );
    let heap = cratonvm_types::flags::with_thread_overrides(&[(KEY_SINK, Some("0"))], || {
        GenerationalHeap::with_sizes(1 << 20, 1 << 20)
    });
    assert!(!heap.tlab_tail_sink_enabled(), "`=0` turns it off");
    let (mut tlab, ptr, size) = armed_tlab(&heap, 64 * 1024);
    assert!(!tlab.has_tail_sink());
    tlab.alloc(128, 8).unwrap();
    let used = young_used(&heap);
    assert!(!heap.return_tlab_tail(ptr + 128, ptr + size));
    tlab.retire();
    assert_eq!(
        young_used(&heap),
        used,
        "the filler keeps the carve occupied"
    );
    // SAFETY: the tail lies inside the carve, which this heap still owns.
    let first = unsafe { std::ptr::read((ptr + 128) as *const u32) };
    assert_eq!(first, cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID.as_u32());
    let s = heap.stats().snapshot();
    assert_eq!(
        (s.young_tlab_tail_returned_bytes, s.young_tlab_tail_declined),
        (0, 0)
    );
    assert!(heap
        .alloc_path_census_line()
        .contains("gen_tlab_tail_sink=0 "));
}

/// THE COMMON CASE: a thread allocates a little and blocks. Its buffer is
/// the last carve, so its tail ends at the bump cursor: the cursor
/// retracts onto it, `used` falls by exactly the tail, no filler is
/// written, and the next carve starts where the thread stopped — from the
/// bump tail, so the one-zeroing-point skip still applies (no repair).
#[test]
fn a_tail_at_the_cursor_retracts_it_and_the_next_carve_reuses_it() {
    let heap = heap_with_sink(true);
    assert!(heap.tlab_tail_sink_enabled());
    let (mut tlab, ptr, size) = armed_tlab(&heap, 64 * 1024);
    assert!(tlab.has_tail_sink());
    tlab.alloc(128, 8).unwrap();
    let used = young_used(&heap);
    let before = heap.stats().snapshot();
    tlab.retire();
    assert!(tlab.is_retired());
    assert_eq!(
        young_used(&heap),
        used - (size - 128),
        "the tail left `used`"
    );
    assert_eq!(
        heap.young_from_published().0,
        used - (size - 128),
        "and the trigger's published triple says so"
    );
    // SAFETY: `[ptr, ptr + size)` is committed young memory.
    let first = unsafe { std::ptr::read((ptr + 128) as *const u32) };
    assert_eq!(first, 0, "no filler over a returned tail");
    let after = heap.stats().snapshot();
    assert_eq!(
        after.young_tlab_tail_retracts,
        before.young_tlab_tail_retracts + 1
    );
    assert_eq!(
        after.young_tlab_tail_returned_bytes,
        before.young_tlab_tail_returned_bytes + (size - 128) as u64
    );
    let (next, _) = heap
        .refill_tlab(64 * 1024)
        .expect("the returned span is free again");
    assert_eq!(
        next as usize,
        ptr + 128,
        "carved exactly where the thread stopped"
    );
    assert_eq!(
        heap.stats().snapshot().young_zero_repairs,
        after.young_zero_repairs,
        "the retracted span still reads zero, so the carve skips its memset cleanly"
    );
    assert!(heap
        .alloc_path_census_line()
        .contains("gen_tlab_tail_retracts=1 "));
}

/// A tail that is NOT at the cursor (another carve came after it) goes on
/// the young free list, which the trigger subtracts; a buffer retired
/// before its first allocation comes back whole and takes its object-start
/// bit with it.
#[test]
fn a_tail_below_the_cursor_joins_the_young_free_list() {
    let heap = heap_with_sink(true);
    let (mut a, a_ptr, a_size) = armed_tlab(&heap, 64 * 1024);
    let (mut b, b_ptr, b_size) = armed_tlab(&heap, 64 * 1024);
    a.alloc(128, 8).unwrap();
    a.retire();
    let (_, free, _) = heap.young_from_published();
    assert_eq!(free, a_size - 128, "A's tail is listed and published");
    assert_eq!(heap.lock_young_from().free_list_bytes(), a_size - 128);
    // SAFETY: A's tail is committed young memory.
    assert_eq!(unsafe { std::ptr::read((a_ptr + 128) as *const u32) }, 0);

    // `Some(true)` when `CRATONVM_GC_OBJECT_STARTS` armed the young
    // bitmap, `None` when it is off (the default).
    assert_ne!(heap.lock_young_from().is_object_start(b_ptr), Some(false));
    let used = young_used(&heap);
    b.retire();
    assert_eq!(young_used(&heap), used - b_size, "B came back whole");
    assert_ne!(
        heap.lock_young_from().is_object_start(b_ptr),
        Some(true),
        "a returned chunk base is no longer an object start"
    );
    let s = heap.stats().snapshot();
    assert_eq!(
        (s.young_tlab_tail_free_listed, s.young_tlab_tail_retracts),
        (1, 1)
    );
}

/// Declines keep the old behaviour exactly: a stray tail below the cursor
/// that is smaller than a minimum TLAB, and any tail offered while the
/// `young_from` lock is held (the retire must never wait for it), get the
/// filler and are counted as declined.
#[test]
fn a_small_stray_tail_or_a_busy_lock_gets_the_filler() {
    let heap = heap_with_sink(true);
    let (mut a, a_ptr, a_size) = armed_tlab(&heap, 64 * 1024);
    let (mut b, b_ptr, _) = armed_tlab(&heap, 64 * 1024);
    let small = cratonvm_gc_core::tlab::min_tlab_size() / 2;
    a.alloc(a_size - small, 8).unwrap();
    a.retire();
    // SAFETY: A's tail is committed young memory.
    let first = unsafe { std::ptr::read((a_ptr + a_size - small) as *const u32) };
    assert_eq!(first, cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID.as_u32());

    b.alloc(128, 8).unwrap();
    let used = young_used(&heap);
    {
        let _held = heap.lock_young_from();
        b.retire();
    }
    assert_eq!(
        young_used(&heap),
        used,
        "a busy lock declines; nothing moved"
    );
    // SAFETY: B's tail is committed young memory.
    let first = unsafe { std::ptr::read((b_ptr + 128) as *const u32) };
    assert_eq!(first, cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID.as_u32());
    let s = heap.stats().snapshot();
    assert_eq!(s.young_tlab_tail_declined, 2);
    assert_eq!(s.young_tlab_tail_returned_bytes, 0);
}

/// A tail that is not this from-space's is declined, whatever the lock.
#[test]
fn a_tail_outside_the_from_space_is_declined() {
    let heap = heap_with_sink(true);
    let mut foreign = vec![0u64; 1024];
    let lo = foreign.as_mut_ptr() as usize;
    assert!(!heap.return_tlab_tail(lo, lo + 4096));
    assert!(!heap.return_tlab_tail(lo + 8, lo + 8));
    assert_eq!(heap.stats().snapshot().young_tlab_tail_declined, 1);
}

/// The stress carve cap: `t / 8` on the grid, clamped to the TLAB bounds.
#[test]
fn the_stress_carve_cap_is_an_eighth_of_t_within_the_tlab_bounds() {
    assert_eq!(stress_tlab_carve_cap(250_000), 31_248);
    assert_eq!(stress_tlab_carve_cap(64), cratonvm_gc_core::tlab::min_tlab_size());
    assert_eq!(stress_tlab_carve_cap(1 << 40), cratonvm_gc_core::tlab::max_tlab_size());
    for t in [1usize, 4096, 262_144, 1 << 20, 3 << 22] {
        assert_eq!(stress_tlab_carve_cap(t) % 8, 0, "t={t}");
    }
}

/// The carve bound, as a pure function: a verdict that direct allocation
/// earned stands; one that only carves produced stands once the carves
/// exceed `t` plus eight capped buffers; nothing fires that the per-epoch
/// verdict did not.
#[test]
fn the_carve_bound_keeps_direct_verdicts_and_holds_carve_only_ones() {
    const T: usize = 250_000;
    let cap = stress_tlab_carve_cap(T);
    let allowance = 8 * cap;
    let base = 400_000;
    let floor = base + T;
    // Not fired: never fires.
    assert!(!gc_stress_carve_bound(false, base + 10 * T, floor, T, 0));
    // All direct.
    assert!(gc_stress_carve_bound(true, base + T, floor, T, 0));
    // Direct `t` on top of some carves.
    let c = 3 * cap;
    assert!(gc_stress_carve_bound(
        true,
        base + T + c,
        floor,
        T,
        c as u64
    ));
    // Two threads' fresh 256 KiB carves, the page's scenario, and nothing
    // else: held while inside the allowance...
    let two = 2 * 131_072;
    assert!(!gc_stress_carve_bound(
        true,
        base + two,
        floor,
        T,
        two as u64
    ));
    // ...and released past it.
    let past = T + allowance;
    assert!(gc_stress_carve_bound(
        true,
        base + past,
        floor,
        T,
        past as u64
    ));
    assert!(!gc_stress_carve_bound(
        true,
        base + past - 8,
        floor,
        T,
        (past - 8) as u64
    ));
    // A returned tail can leave the metric below the epoch's base: no
    // underflow, no fire on carves alone.
    assert!(!gc_stress_carve_bound(true, base - 8, floor, T, cap as u64));    }