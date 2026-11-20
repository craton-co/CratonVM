use super::*;
use crate::threading::jvm_thread::{ThreadId, AGENT_REQUEST_FRAME_CONTROL};
use std::sync::atomic::Ordering;

fn frame() -> Frame {
    Frame::new(
        ClassId::new(0),
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    )
}

#[test]
fn a_raised_request_is_taken_once_for_the_top_frame() {
    let mut thread = JvmThread::new(ThreadId(9501), "agent-word");
    thread.frames.push(frame());
    thread.frames.push(frame());
    assert!(!take_method_entry_frame_control(&thread, 1), "nothing raised");
    thread
        .gc_block_state
        .agent_requests
        .fetch_or(AGENT_REQUEST_FRAME_CONTROL, Ordering::Relaxed);
    assert!(take_method_entry_frame_control(&thread, 1), "the top frame takes it");
    assert_eq!(
        thread.gc_block_state.agent_requests.load(Ordering::Relaxed),
        0,
        "taken: cleared"
    );
    assert!(!take_method_entry_frame_control(&thread, 1), "taken once");
}

#[test]
fn a_request_seen_below_the_top_frame_is_dropped() {
    let mut thread = JvmThread::new(ThreadId(9502), "agent-word-below");
    thread.frames.push(frame());
    thread.frames.push(frame());
    thread
        .gc_block_state
        .agent_requests
        .fetch_or(AGENT_REQUEST_FRAME_CONTROL, Ordering::Relaxed);
    assert!(!take_method_entry_frame_control(&thread, 0));
    assert_eq!(thread.gc_block_state.agent_requests.load(Ordering::Relaxed), 0);
}
