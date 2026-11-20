use super::{assert_no_compiled_continuation, thread_is_continuation};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::sync::atomic::Ordering;

#[test]
fn a_jdk_continuation_enters_compiled_code_and_is_marked_once() {
    let mut thread = JvmThread::new(ThreadId(115), "i115-l6-jdk-continuation");
    // A platform thread: no mark, no assertion.
    assert_no_compiled_continuation(&thread);
    assert!(!thread
        .gc_block_state
        .continuation_entered_compiled
        .load(Ordering::Relaxed));
    thread.kind = crate::threading::ThreadKind::Virtual;
    thread.jdk_continuation.set_jdk();
    assert!(thread_is_continuation(&thread));
    // Twice: the second entry finds the mark already set.
    assert_no_compiled_continuation(&thread);
    assert_no_compiled_continuation(&thread);
    assert!(thread
        .gc_block_state
        .continuation_entered_compiled
        .load(Ordering::Relaxed));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "is entering compiled code")]
fn a_bound_virtual_thread_still_trips_the_debug_assertion() {
    let mut thread = JvmThread::new(ThreadId(116), "i115-l6-bound-continuation");
    thread.kind = crate::threading::ThreadKind::Virtual;
    assert_no_compiled_continuation(&thread);
}
