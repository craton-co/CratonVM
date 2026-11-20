use super::{jit_ic_grace_catch_up, JIT_RETIRE_GENERATION, THREAD_QUIESCENCE};
use std::sync::atomic::Ordering;

fn own_ic_stamp() -> u64 {
    THREAD_QUIESCENCE.with(|h| h.record.ic_quiescent_gen.load(Ordering::Acquire))
}

#[test]
fn a_catch_up_vouches_for_the_refusing_thread_first() {
    THREAD_QUIESCENCE.with(|h| {
        h.ic_stamp_flag.set(2);
        h.ic_catch_up_self_stamp_flag.set(2);
    });
    assert_eq!(
        own_ic_stamp(),
        0,
        "a fresh thread has no miss-handler stamp"
    );
    let before = JIT_RETIRE_GENERATION.load(Ordering::Acquire);
    let _ = jit_ic_grace_catch_up();
    assert!(
        own_ic_stamp() >= before,
        "the refusing thread must not bound its own catch-up with a stale stamp"
    );
}

#[test]
fn the_kill_switch_leaves_the_stamp_to_the_miss_handlers() {
    THREAD_QUIESCENCE.with(|h| {
        h.ic_stamp_flag.set(2);
        h.ic_catch_up_self_stamp_flag.set(1);
    });
    let _ = jit_ic_grace_catch_up();
    assert_eq!(own_ic_stamp(), 0);
}
