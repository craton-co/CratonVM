use super::*;

/// `jdk.internal.misc.VM.toThreadState(int)`, verbatim.
fn to_thread_state(status: i32) -> &'static str {
    if status & 0x0004 != 0 {
        "RUNNABLE"
    } else if status & 0x0400 != 0 {
        "BLOCKED"
    } else if status & 0x0010 != 0 {
        "WAITING"
    } else if status & 0x0020 != 0 {
        "TIMED_WAITING"
    } else if status & 0x0002 != 0 {
        "TERMINATED"
    } else if status & 0x0001 == 0 {
        "NEW"
    } else {
        "RUNNABLE"
    }
}

#[test]
fn the_status_words_decode_to_the_registry_states() {
    assert_eq!(to_thread_state(0), "NEW");
    for (java_state, want) in [
        (0u8, "RUNNABLE"),
        (1, "WAITING"),
        (2, "BLOCKED"),
        (3, "TIMED_WAITING"),
        (JAVA_STATE_TERMINATING, "TERMINATED"),
        (200, "RUNNABLE"),
    ] {
        assert_eq!(
            to_thread_state(thread_status_word_for_java_state(java_state)),
            want,
            "java_state {java_state}"
        );
    }
    assert_eq!(to_thread_state(THREAD_STATUS_RUNNABLE), "RUNNABLE");
    assert_eq!(to_thread_state(THREAD_STATUS_TERMINATED), "TERMINATED");
}

#[test]
fn a_live_eetop_is_nonzero_and_distinct_per_thread() {
    assert_ne!(live_eetop(ThreadId(0)), 0);
    assert_ne!(live_eetop(ThreadId(0)), live_eetop(ThreadId(1)));
}
