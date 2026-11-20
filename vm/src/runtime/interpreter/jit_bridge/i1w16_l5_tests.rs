use crate::config::VmConfig;
use crate::runtime::frame::{Frame, OSR_MAX_ATTEMPTS};
use crate::runtime::interpreter::{
    osr_next_poll, osr_poll_point_armed, try_osr_with_backoff, OsrBackoffOutcome,
    OSR_POLL_STRIDE,
};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

fn loop_frame(method: &str) -> Frame {
    Frame::new(
        ClassId::new(0x9b01),
        "cratonvm/test/OsrW16Holder".to_string(),
        method.to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        1,
        &[],
    )
}

#[test]
fn the_next_poll_is_the_headers_due_point_or_a_stride_away() {
    let t = 1_000;
    let mut f = loop_frame("due");
    f.backward_count = 1_000;
    assert!(
        osr_next_poll(&f, 4, t) <= f.backward_count,
        "a header never asked is due now: poll on the next back edge"
    );
    f.set_osr_attempts(4, 0);
    assert!(osr_next_poll(&f, 4, t) <= f.backward_count);
    f.set_osr_attempts(4, 1);
    assert_eq!(
        osr_next_poll(&f, 4, t),
        1_000 + OSR_POLL_STRIDE,
        "waiting on the backoff (due at 2000): a stride first"
    );
    f.backward_count = 1_900;
    assert_eq!(osr_next_poll(&f, 4, t), 2_000, "then the due point itself");
    f.set_osr_attempts(4, OSR_MAX_ATTEMPTS);
    assert_eq!(
        osr_next_poll(&f, 4, t),
        1_900 + OSR_POLL_STRIDE,
        "a retired header: once per stride"
    );
    f.backward_count = u32::MAX - 1;
    assert_eq!(
        osr_next_poll(&f, 4, t),
        0,
        "near the top of the counter: every back edge until it wraps"
    );
}

/// A frame with one loop is offered at exactly the back edges it was
/// offered at before the poll point existed, with a fraction of the calls.
#[test]
fn a_single_loop_is_offered_at_the_same_back_edges_with_fewer_calls() {
    let t = 1_000;
    let run = |poll_point: bool| {
        let mut f = loop_frame("single");
        let (mut offers, mut calls) = (Vec::new(), 0u32);
        for bc in 1..=40_000u32 {
            f.backward_count = bc;
            if bc < t || (poll_point && bc < f.osr_poll_at) {
                continue;
            }
            calls += 1;
            if f.should_try_osr(0, t) {
                offers.push(bc);
                // Every offer refused, as `charge_osr_rejection(.., 1)`.
                let n = f.osr_attempts_at(0).unwrap_or(0);
                f.set_osr_attempts(0, n + 1);
            }
            if poll_point {
                f.osr_poll_at = osr_next_poll(&f, 0, t);
            }
        }
        (offers, calls)
    };
    let (before, calls_before) = run(false);
    let (after, calls_after) = run(true);
    assert_eq!(before, vec![1_000, 2_000, 4_000, 8_000, 16_000]);
    assert_eq!(after, before, "the same offers at the same back edges");
    assert_eq!(calls_before, 39_001);
    assert!(
        calls_after < 200,
        "{calls_after} calls: one per stride once the header is retired"
    );
}

/// The interpreter's own funnel leaves the poll point on the frame when it
/// declines a retired header.
#[test]
fn a_declined_offer_leaves_the_frames_next_poll() {
    if !crate::runtime::env_cache::osr_backedge_enabled()
        || crate::runtime::env_cache::disable_jit()
        || !osr_poll_point_armed()
    {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "osr-poll-point");
    thread.frames.push(loop_frame("retired"));
    thread.frames[0].set_osr_attempts(0, OSR_MAX_ATTEMPTS);
    thread.frames[0].backward_count = 5_000;
    let mut idx = 0;
    let out = try_osr_with_backoff(&shared, &mut thread, &mut idx, 0, 0);
    assert!(matches!(out, OsrBackoffOutcome::Skip));
    assert_eq!(thread.frames[0].osr_poll_at, 5_000 + OSR_POLL_STRIDE);
    let mut reused = loop_frame("reused");
    reused.osr_poll_at = 7;
    reused.record_osr_background_pending();
    assert_eq!(
        reused.osr_poll_at, 0,
        "a restarted stride restarts the poll point"
    );
}
