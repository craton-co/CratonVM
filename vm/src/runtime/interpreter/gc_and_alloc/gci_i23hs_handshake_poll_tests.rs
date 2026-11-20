use super::*;
use crate::threading::jvm_thread::HandshakeState;
use std::sync::atomic::Ordering;

/// gci i23/hs (step 1): `safepoint_check` with no pause requested runs
/// the op queued for its thread, on that thread, exactly once.
#[test]
fn gci_i23hs_safepoint_check_serves_a_queued_handshake() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(crate::ThreadId(9123), "i23hs-poll");
    let ticket = thread
        .gc_block_state
        .handshakes
        .push(Box::new(|_: &SharedVm, t: &JvmThread| {
            t.root_publish_seq.fetch_add(1, Ordering::SeqCst);
        }));
    assert!(!shared.mem.gc_barrier.stw_requested.load(Ordering::Acquire));
    safepoint_check(&shared, &mut thread);
    assert_eq!(thread.root_publish_seq.load(Ordering::SeqCst), 1);
    assert_eq!(ticket.state(), HandshakeState::Ran);
    assert!(!thread.gc_block_state.handshakes.is_pending());
    safepoint_check(&shared, &mut thread);
    assert_eq!(thread.root_publish_seq.load(Ordering::SeqCst), 1, "never twice");
}

/// gci i23/hs (step 2): the frame-trace op runs on the thread, publishes
/// its live trace (no frames here: empty) and bumps `root_publish_seq`
/// once.
#[test]
fn gci_i23hs_frame_trace_op_publishes_and_bumps_the_publish_seq() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(crate::ThreadId(9124), "i23hs-op");
    let seq = thread.root_publish_seq.load(Ordering::Relaxed);
    let ticket = thread.gc_block_state.handshakes.push(frame_trace_handshake_op());
    thread.run_pending_handshakes(&shared);
    assert_eq!(ticket.state(), HandshakeState::Ran);
    assert_eq!(thread.root_publish_seq.load(Ordering::Relaxed), seq + 1);
    assert!(thread.frame_trace.lock().is_empty());
}

/// gci i23/hs: the poll sits before the pause branch of `safepoint_check`
/// (a requester withdraws what is left once a pause is requested), and
/// the pause arrival publishes through the same function the op runs.
#[test]
fn gci_i23hs_the_poll_precedes_the_pause_branch() {
    let src = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs")).replace("\r\n", "\n");
    let start = src
        .find("\npub(crate) fn safepoint_check(")
        .expect("safepoint_check");
    let body = &src[start..];
    let body = &body[..body.find("\n\u{7d}\n").expect("its end")];
    let poll = body
        .find("thread.run_pending_handshakes(shared);")
        .expect("the handshake poll");
    let pause = body
        .find("if shared.mem.gc_barrier.stw_requested.load(Ordering::Acquire)")
        .expect("the pause branch");
    assert!(poll < pause);
    assert!(body.contains("publish_live_frame_trace(shared, thread);"));
}

/// gcj j3/rt3 residual 4: the owner is marked as inside an op while it
/// runs one (debug builds), and only then.
#[test]
fn gcj3rt3_a_running_handshake_op_is_marked() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(crate::ThreadId(9125), "rt3-mark");
    let mark = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mark_clone = std::sync::Arc::clone(&mark);
    let ticket =
        thread
            .gc_block_state
            .handshakes
            .push(Box::new(move |_: &SharedVm, t: &JvmThread| {
                let inside = t.gc_block_state.handshakes.op_running();
                mark_clone.store(inside, std::sync::atomic::Ordering::SeqCst);
            }));
    assert!(!thread.gc_block_state.handshakes.op_running());
    thread.run_pending_handshakes(&shared);
    assert_eq!(ticket.state(), HandshakeState::Ran);
    let expect = cfg!(debug_assertions);
    assert_eq!(mark.load(std::sync::atomic::Ordering::SeqCst), expect);
    assert!(
        !thread.gc_block_state.handshakes.op_running(),
        "cleared after"
    );
}

/// gcj j3/rt3 residual 4, fail-first: an op that reaches a pause arrival
/// (`safepoint_check` with a pause requested) fails in a debug build
/// instead of holding the pause for its requester's 1 s backstop; the
/// unwind settles its ticket and clears the mark.
#[cfg(debug_assertions)]
#[test]
fn gcj3rt3_a_handshake_op_that_waits_for_a_pause_fails_in_debug() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(crate::ThreadId(9126), "rt3-violate");
    let ticket =
        thread
            .gc_block_state
            .handshakes
            .push(Box::new(|vm: &SharedVm, t: &JvmThread| {
                assert!(vm.mem.gc_barrier.request_stw(crate::ThreadId(9127), 1));
                t.gc_block_state.handshakes.debug_assert_no_op_running();
            }));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        thread.run_pending_handshakes(&shared);
    }));
    shared
        .mem
        .gc_barrier
        .complete_gc(cratonvm_types::PointerMap::default());
    let msg = caught.expect_err("the arrival asserts");
    let msg = msg
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| msg.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert!(
        msg.contains("HandshakeOp waited for a stop-the-world pause"),
        "{msg}"
    );
    assert_eq!(ticket.state(), HandshakeState::Abandoned);
    assert!(!thread.gc_block_state.handshakes.op_running());
}
