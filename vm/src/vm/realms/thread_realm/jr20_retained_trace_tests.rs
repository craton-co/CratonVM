use super::{ThrowableLayoutCache, RETAINED_TRACE_FRAMES_GC_BUDGET};

/// JIT round 20 (lane throw): the registry's budget asks once per budget
/// of stored frames, consumes the count when it asks, and a sweep resets it.
#[test]
fn retained_trace_pressure_asks_once_per_budget() {
    let cache = ThrowableLayoutCache::new();
    assert!(!cache.take_retained_trace_pressure());
    cache.note_retained_trace_frames(RETAINED_TRACE_FRAMES_GC_BUDGET - 1);
    assert!(!cache.take_retained_trace_pressure());
    cache.note_retained_trace_frames(1);
    assert!(cache.take_retained_trace_pressure());
    assert!(!cache.take_retained_trace_pressure(), "consumed");
    cache.note_retained_trace_frames(RETAINED_TRACE_FRAMES_GC_BUDGET - 1);
    cache.reset_retained_trace_frames();
    cache.note_retained_trace_frames(1);
    assert!(
        !cache.take_retained_trace_pressure(),
        "a sweep reset the count"
    );
}
