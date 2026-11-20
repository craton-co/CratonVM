use super::*;
use std::sync::atomic::Ordering;

/// These tests exercise one VM's demand independently of other heaps.
static FRAME_TRACE_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// The request flag must read `false` when nobody is dumping.
///
/// This is the whole cost argument for gating the publish in
/// `safepoint_check`: raised, every mutator captures and allocates a frame
/// trace on every GC pause. A flag that latched on would not fail any test
/// — it would just quietly make every pause more expensive — so assert the
/// resting state explicitly.
#[test]
fn the_request_flag_rests_low() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let _serial = FRAME_TRACE_TEST_LOCK.lock();
    assert!(
        !frame_trace_wanted(&shared),
        "FRAME_TRACE_WANTED must rest low; raised, every safepoint park pays \
         for a frame-trace capture nobody asked for"
    );
}

/// And it must come back down when the pause ends — including the path
/// where no pause was taken.
///
/// Exercise the request guard's early exits. Breaking `Drop` (or replacing it with a
/// hand-written decrement that an early `return` can skip) fails this.
#[test]
fn the_request_guard_lowers_the_flag_on_every_path() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let _serial = FRAME_TRACE_TEST_LOCK.lock();
    fn take(shared: &SharedVm, early_out: bool) -> bool {
        let _wanted = FrameTraceWanted::raise(&shared.mem.diagnostics.frame_trace_wanted);
        assert!(
            frame_trace_wanted(&shared),
            "the flag must be observable while raised"
        );
        !early_out
    }
    for early_out in [true, false] {
        let _ = take(&shared, early_out);
        assert!(
            !frame_trace_wanted(&shared),
            "the flag stayed raised after the early_out={early_out} path"
        );
    }
    assert_eq!(
        shared
            .mem
            .diagnostics
            .frame_trace_wanted
            .load(Ordering::Relaxed),
        0
    );
}

/// gc-common w4-a (`handoff-w3a-frame-trace-wanted-counter`): two
/// concurrent requesters. The LOSER (its request declined) drops its raise
/// while the WINNER's pause is still collecting traces; the flag must stay
/// raised until the winner drops too. With the old bool the loser's guard
/// stored `false` here and every mutator parking afterwards published no
/// trace.
#[test]
fn a_losing_requester_does_not_lower_the_winners_flag() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let _serial = FRAME_TRACE_TEST_LOCK.lock();
    let winner = FrameTraceWanted::raise(&shared.mem.diagnostics.frame_trace_wanted);
    let loser = FrameTraceWanted::raise(&shared.mem.diagnostics.frame_trace_wanted);
    drop(loser);
    assert!(
        frame_trace_wanted(&shared),
        "the loser's drop lowered the flag under the winner's pause"
    );
    drop(winner);
    assert!(!frame_trace_wanted(&shared));
}

/// Interpreter round i1 wave 14, lane L3: the frame-trace pause is a
/// handshake pause. With every counted mutator arriving inside the grace
/// slice (here: none to wait for) it runs no take-over, reopens the world
/// through the pause's `Drop` (a second dump is granted), and lowers the
/// request flag.
#[test]
fn a_frame_trace_pause_every_mutator_reaches_reopens_the_world() {
    let _serial = FRAME_TRACE_TEST_LOCK.lock();
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let outsider = crate::ThreadId(u64::MAX);
    assert!(stw_publish_frame_traces(&shared, outsider));
    assert!(!frame_trace_wanted(&shared));
    assert!(
        stw_publish_frame_traces(&shared, outsider),
        "the first pause left the world stopped"
    );
    assert!(!frame_trace_wanted(&shared));
    // Frozen by nobody: the handshake arm the grace slice decides.
    let pause = NonMovingPause::request_handshake(
        &shared,
        outsider,
        NonCollectionPause::FrameTrace,
        FRAME_TRACE_GRACE,
    )
    .expect("no other pause owns the world");
    assert!(!pause.froze_peers());
}

/// The no-roots scope is thread-local, rests OFF, and is lowered on every
/// path out -- including an unwind.
///
/// Its failure mode is the dangerous one: a take-over that believes it
/// needs no roots, run for a real COLLECTION, would sweep every object only
/// a frozen or blocked peer names. So the resting state and the scoping are
/// asserted rather than assumed.
#[test]
fn the_no_roots_scope_is_scoped_to_one_thread_and_one_call() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    assert!(
        takeover_gathers_roots(),
        "a take-over gathers roots by default"
    );
    {
        let _g = NoRootsPause::enter();
        assert!(!takeover_gathers_roots());
        // Another thread -- another initiator -- is unaffected.
        let other = std::thread::spawn(takeover_gathers_roots).join().unwrap();
        assert!(other, "the scope must not leak to another thread");
    }
    assert!(takeover_gathers_roots(), "lowered when the scope ends");
    let unwound = std::panic::catch_unwind(|| {
        let _g = NoRootsPause::enter();
        panic!("unwind through the scope");
    });
    assert!(unwound.is_err());
    assert!(takeover_gathers_roots(), "lowered on unwind too");
}
