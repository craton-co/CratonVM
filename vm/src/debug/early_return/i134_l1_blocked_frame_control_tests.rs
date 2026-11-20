use super::*;
use crate::classloading::ClassId;
use crate::runtime::frame::Frame;
use crate::threading::jvm_thread::{JvmThread, ThreadId, AGENT_REQUEST_FRAME_CONTROL};
use std::sync::atomic::Ordering;

fn frame(name: &str) -> Frame {
    Frame::new(
        ClassId::new(0),
        "T".to_string(),
        name.to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    )
}

fn blocked_thread(tid: u64) -> JvmThread {
    let mut thread = JvmThread::new(ThreadId(tid), "blocked-entry");
    thread.frames.push(frame("caller"));
    thread.frames.push(frame("entry"));
    thread
}

fn standing(shared: &crate::vm::SharedVm, tid: u64) -> Option<crate::debug::DeferredFrameControl> {
    deferred_control(&shared.debug.debug_state.lock(), tid)
}

/// `INTERNAL` before `OPAQUE_FRAME` before `TYPE_MISMATCH`, as
/// `SetForceEarlyReturn::doit` checks them.
#[test]
fn a_blocked_forced_return_is_refused_in_hotspots_order() {
    let forced = crate::debug::DeferredFrameControl {
        frame_count: 2,
        class_id: 0,
        method_id: 0,
        pop: None,
        forced: Some(JvmtiForced::Void),
        forced_after_pop: false,
    };
    assert_eq!(blocked_forced_refusal(true, None, false, false), Err(ERR_INTERNAL));
    assert_eq!(
        blocked_forced_refusal(false, Some(forced), false, false),
        Err(ERR_INTERNAL)
    );
    assert_eq!(blocked_forced_refusal(false, None, false, false), Err(ERR_OPAQUE_FRAME));
    assert_eq!(blocked_forced_refusal(false, None, true, false), Err(ERR_TYPE_MISMATCH));
    assert_eq!(blocked_forced_refusal(false, None, true, true), Ok(()));
    // A deferred pop alone does not refuse a forced return.
    let popped = crate::debug::DeferredFrameControl {
        pop: Some(true),
        forced: None,
        ..forced
    };
    assert_eq!(blocked_forced_refusal(false, Some(popped), true, true), Ok(()));
}

/// The pop goes to the wake's depth; the forced return too, or to the
/// caller's when it was made after the pop.
#[test]
fn adopted_records_land_where_a_callback_points_would() {
    let mut c = crate::debug::DeferredFrameControl {
        frame_count: 3,
        class_id: 0,
        method_id: 0,
        pop: None,
        forced: Some(JvmtiForced::Void),
        forced_after_pop: false,
    };
    assert_eq!(adopted_depths(&c, 3), (None, Some(3)));
    c.pop = Some(false);
    assert_eq!(adopted_depths(&c, 3), (Some((3, false)), Some(3)));
    c.forced_after_pop = true;
    assert_eq!(adopted_depths(&c, 3), (Some((3, false)), Some(2)));
    c.forced = None;
    assert_eq!(adopted_depths(&c, 3), (Some((3, false)), None));
}

/// One pop and one forced return per suspension; the wake of the same
/// frame adopts both and raises the agent-request word.
#[test]
fn a_blocked_request_is_recorded_once_and_adopted_at_the_wake() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let tid = 13_401;
    let thread = blocked_thread(tid);
    assert_eq!(defer_blocked_pop(&shared, tid, &thread.frames, Ok(true)), Ok(()));
    assert_eq!(
        defer_blocked_pop(&shared, tid, &thread.frames, Ok(true)),
        Err(ERR_INTERNAL),
        "a second PopFrame before the resume"
    );
    let forced = |kind, value| {
        defer_blocked_forced_return(&shared, tid, &thread.frames, true, kind, value)
    };
    assert_eq!(
        forced(b'I', JvmtiForced::Plain(Value::Int(1))),
        Err(ERR_TYPE_MISMATCH),
        "an int for a void method"
    );
    assert_eq!(forced(b'V', JvmtiForced::Void), Ok(()));
    assert_eq!(forced(b'V', JvmtiForced::Void), Err(ERR_INTERNAL));
    let control = standing(&shared, tid).expect("recorded");
    assert_eq!(
        (control.frame_count, control.pop, control.forced_after_pop),
        (2, Some(true), true)
    );
    assert!(shared.debug.debugger_gates.frame_writes_pending());
    assert!(!jvmti_frame_control_at(&shared, tid, 2), "not before the wake");
    {
        let mut ds = shared.debug.debug_state.lock();
        let pops = ds.deferred_frame_pops.remove(&tid).expect("pops");
        adopt_blocked_frame_control(&shared, &mut ds, &thread, pops.control.expect("control"));
        assert_eq!(ds.early_returns.jvmti_pops.get(&tid), Some(&(2, true)));
        assert_eq!(ds.early_returns.jvmti_pending.get(&tid), Some(&(1, JvmtiForced::Void)));
    }
    assert!(jvmti_frame_control_at(&shared, tid, 2));
    assert_ne!(
        thread.gc_block_state.agent_requests.load(Ordering::Relaxed) & AGENT_REQUEST_FRAME_CONTROL,
        0,
        "the loop's reader is raised"
    );
}

/// Interpreter round i7 wave 135, lane L1: a blocked `PopFrame` the
/// taking loop drops (its entry frame) moves the forced return made after
/// it from the caller's depth to the frame that stays on top, instead of
/// leaving it to stand forever.
#[test]
fn i135_l1_a_dropped_blocked_pop_moves_the_forced_return_to_the_top() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let tid = 13_501;
    let thread = blocked_thread(tid);
    assert_eq!(defer_blocked_pop(&shared, tid, &thread.frames, Ok(true)), Ok(()));
    assert_eq!(
        defer_blocked_forced_return(&shared, tid, &thread.frames, true, b'V', JvmtiForced::Void),
        Ok(())
    );
    {
        let mut ds = shared.debug.debug_state.lock();
        let pops = ds.deferred_frame_pops.remove(&tid).expect("pops");
        adopt_blocked_frame_control(&shared, &mut ds, &thread, pops.control.expect("control"));
        assert_eq!(ds.early_returns.jvmti_pending.get(&tid), Some(&(1, JvmtiForced::Void)));
    }
    assert!(crate::debug::pop_frames::drop_jvmti_pop_at(&shared, tid, 2));
    let ds = shared.debug.debug_state.lock();
    assert!(ds.early_returns.jvmti_pops.get(&tid).is_none());
    assert_eq!(ds.early_returns.jvmti_pending.get(&tid), Some(&(2, JvmtiForced::Void)));
    drop(ds);
    // Nothing to move: a forced return of the top frame itself stays.
    let mut ds = shared.debug.debug_state.lock();
    assert!(!rekey_jvmti_forced_after_dropped_pop(&mut ds, tid, 2));
    assert!(!rekey_jvmti_forced_after_dropped_pop(&mut ds, 13_502, 2));
}

/// A wake whose top frame is not the one the request named drops it.
#[test]
fn a_wake_of_another_frame_drops_the_request() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let tid = 13_402;
    let mut thread = blocked_thread(tid);
    assert_eq!(defer_blocked_pop(&shared, tid, &thread.frames, Ok(false)), Ok(()));
    let control = standing(&shared, tid).expect("recorded");
    thread.frames.push(frame("deeper"));
    let mut ds = shared.debug.debug_state.lock();
    adopt_blocked_frame_control(&shared, &mut ds, &thread, control);
    assert!(ds.early_returns.jvmti_pops.get(&tid).is_none());
    drop(ds);
    assert_eq!(
        thread.gc_block_state.agent_requests.load(Ordering::Relaxed) & AGENT_REQUEST_FRAME_CONTROL,
        0
    );
    // A refusal the caller computed is answered after the pending test.
    assert_eq!(
        defer_blocked_pop(&shared, 13_403, &thread.frames, Err(ERR_OPAQUE_FRAME)),
        Err(ERR_OPAQUE_FRAME)
    );
    assert!(standing(&shared, 13_403).is_none(), "nothing recorded on refusal");
}

/// Interpreter round i7 wave 136, lane L1: the target depth is the pop's
/// when one is recorded; a stale pop drops the forced return made after
/// it; a record at or below the depth stays.
#[test]
fn i136_l1_stale_frame_control_is_dropped_above_a_depth() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let tid = 13_601;
    assert_eq!(jvmti_frame_control_depth(&shared, tid), None);
    {
        let mut ds = shared.debug.debug_state.lock();
        ds.early_returns.jvmti_pops.insert(tid, (4, false));
        ds.early_returns.jvmti_pending.insert(tid, (3, JvmtiForced::Void));
    }
    assert_eq!(jvmti_frame_control_depth(&shared, tid), Some(4), "the pop's");
    assert!(!discard_jvmti_frame_control_above(&shared, tid, 4), "nothing above 4");
    assert_eq!(jvmti_frame_control_depth(&shared, tid), Some(4));
    assert!(discard_jvmti_frame_control_above(&shared, tid, 3));
    assert_eq!(
        jvmti_frame_control_depth(&shared, tid),
        None,
        "the forced return made after the dropped pop goes with it"
    );
    shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .jvmti_pending
        .insert(tid, (2, JvmtiForced::Plain(Value::Int(7))));
    assert_eq!(jvmti_frame_control_depth(&shared, tid), Some(2), "the forced return's");
    assert!(!discard_jvmti_frame_control_above(&shared, tid, 2));
    assert!(discard_jvmti_frame_control_above(&shared, tid, 1));
    assert_eq!(jvmti_frame_control_depth(&shared, tid), None);
    // A thread's end drops what it never took, and its parks.
    enter_point(&shared, tid, 2, 0);
    {
        let mut ds = shared.debug.debug_state.lock();
        ds.early_returns.jvmti_pops.insert(tid, (2, true));
        ds.early_returns.jvmti_pending.insert(tid, (1, JvmtiForced::Void));
        forget_thread(&shared, &mut ds, tid);
        assert!(innermost_point(&ds, tid).is_none());
    }
    assert_eq!(jvmti_frame_control_depth(&shared, tid), None);
}
