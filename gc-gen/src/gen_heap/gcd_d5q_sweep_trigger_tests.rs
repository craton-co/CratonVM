//! gcd d5/q, kept by gcg g6/yg when the sweep's own loop was deleted: a
//! sweep's measured pause feeds the moving loop.
use super::*;

#[test]
fn off_the_sweep_pause_feeds_the_moving_loop_as_before() {
    let cap = 1usize << 20;
    let heap = GenerationalHeap::with_sizes(cap, cap);
    let moving_before = heap.young_trigger_census().trigger_bytes;
    let count = heap.stats.minor_gc_count.load(Ordering::Relaxed);
    heap.non_moving_latch[0].store(count, Ordering::Relaxed);
    cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(false);
    heap.adapt_young_trigger_to_pause(300, 200, 0);
    assert_eq!(
        heap.young_trigger_census().trigger_bytes,
        (moving_before / 2).max(cap / 16)
    );
}
