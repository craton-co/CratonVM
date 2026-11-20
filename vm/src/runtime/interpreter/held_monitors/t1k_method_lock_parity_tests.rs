use super::{
    check_structured_return, monitorexit_unrecorded, release_method_monitor_at_return,
    HeldMonitors,
};
use crate::config::VmConfig;
use crate::runtime::frame::Frame;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use std::sync::Arc;

/// `synchronized void m()` = `aload_0; monitorexit; return`, stopped at
/// its `monitorexit` (unbalanced: an exit no `monitorenter` of the frame
/// took).
fn frame(thread: &mut JvmThread) -> Frame {
    let mut f = Frame::new_pooled(
        cratonvm_types::ClassId::new(0),
        Arc::from("T"),
        Arc::from("m"),
        Arc::from("()V"),
        None,
        Arc::from(&[0x2au8, 0xc3, 0xb1][..]),
        Arc::from(Vec::new().into_boxed_slice()),
        4,
        4,
        &[],
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    f.last_instr_pc = 1;
    f
}

#[test]
fn the_exited_bit_rides_the_mark_and_survives_a_freeze() {
    let mut h = HeldMonitors::new();
    assert!(h.is_empty() && !h.method_monitor_exited());
    h.set_method_monitor_exited();
    assert!(!h.is_empty(), "the return and unwind checks must ask");
    assert_eq!(h.len(), 0);
    assert!(h.as_slice().is_empty(), "never a GC root");
    assert!(!h.is_understated());
    h.mark_understated();
    assert!(h.method_monitor_exited() && h.is_understated());
    let thawed = HeldMonitors::from_frozen(&[], false, h.mark_code());
    assert!(thawed.method_monitor_exited() && thawed.is_understated());
    assert!(h.take_method_monitor_exited());
    assert!(!h.method_monitor_exited() && h.is_understated() && h.is_empty());
    let mut g = HeldMonitors::new();
    g.set_method_monitor_exited();
    assert!(g.take_method_monitor_exited());
    assert!(g.is_empty() && g.mark_code().is_none(), "a bit-only mark goes");
    assert!(!g.take_method_monitor_exited());
}

#[test]
fn a_frames_own_exit_of_its_method_monitor_releases_it_and_the_return_throws() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(301), "t1k-method-lock");
    let obj = shared.mem.heap.alloc_object(cratonvm_types::ClassId::new(7), 0);
    shared.threads.monitors.enter(obj, thread.thread_id);
    let mut f = frame(&mut thread);
    f.monitor_on_exit = Some(obj);
    thread.frames.push(f);
    assert!(monitorexit_unrecorded(&shared, &mut thread, 0, obj).is_ok(), "HotSpot unlocks");
    assert!(!shared.threads.monitors.holds(obj, thread.thread_id));
    assert!(thread.frames[0].monitor_on_exit.is_none());
    assert!(thread.frames[0].held_monitors.method_monitor_exited());
    assert!(check_structured_return(&shared, &mut thread, 0).is_err(), "IMSE at the return");
    assert!(check_structured_return(&shared, &mut thread, 0).is_err(), "and again");
}

#[test]
fn a_method_monitor_is_released_at_the_return_or_throws_when_not_owned() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(302), "t1k-method-lock-return");
    let obj = shared.mem.heap.alloc_object(cratonvm_types::ClassId::new(7), 0);
    // Owned: released at the return, nothing thrown.
    shared.threads.monitors.enter(obj, thread.thread_id);
    let mut f = frame(&mut thread);
    f.monitor_on_exit = Some(obj);
    thread.frames.push(f);
    assert!(release_method_monitor_at_return(&shared, &mut thread, 0).is_none());
    assert!(!shared.threads.monitors.holds(obj, thread.thread_id));
    assert!(thread.frames[0].monitor_on_exit.is_none());
    assert!(!thread.frames[0].held_monitors.method_monitor_exited());
    // Not owned: IMSE, remembered for a second return.
    thread.frames[0].monitor_on_exit = Some(obj);
    assert!(release_method_monitor_at_return(&shared, &mut thread, 0).is_some());
    assert!(thread.frames[0].monitor_on_exit.is_none());
    assert!(thread.frames[0].held_monitors.method_monitor_exited());
}

/// t1 wave 4, lane V3: a decoded return whose block record held only stale
/// entries (pruned by the return check) still releases the method monitor
/// at the return, before `MethodExit`, as the raw-byte arms do when they
/// re-dispatch the return; it was left to the pop.
#[test]
fn a_return_with_only_stale_block_entries_releases_the_method_monitor() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(303), "t1v3-stale-record");
    let obj = shared.mem.heap.alloc_object(cratonvm_types::ClassId::new(7), 0);
    let stale = shared.mem.heap.alloc_object(cratonvm_types::ClassId::new(7), 0);
    shared.threads.monitors.enter(obj, thread.thread_id);
    let mut f = frame(&mut thread);
    f.monitor_on_exit = Some(obj);
    f.held_monitors.push(stale); // never entered: the table does not back it
    thread.frames.push(f);
    assert!(check_structured_return(&shared, &mut thread, 0).is_ok());
    assert!(thread.frames[0].held_monitors.is_empty(), "the stale entry is pruned");
    assert!(thread.frames[0].monitor_on_exit.is_none(), "released at the return");
    assert!(!shared.threads.monitors.holds(obj, thread.thread_id));
}

/// t1 wave 4, lane V3: a JDWP / JVMTI forced early return releases the
/// method monitor before `MethodExit` too, as the return arms do (HotSpot's
/// early-return entry runs the same `remove_activation`).
#[test]
fn a_forced_early_return_releases_the_method_monitor_before_method_exit() {
    let src = include_str!("../jvmti_events.rs");
    let start = src
        .find("pub(super) fn finish_forced_return(")
        .expect("the forced-return finisher");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    let release = body
        .find("held_monitors::release_method_monitor_at_return(shared, thread, frame_idx)")
        .expect("the method-monitor release");
    let exit = body
        .find("fire_method_exit_keeping_value(shared, thread, frame_idx, &mut value)")
        .expect("MethodExit");
    assert!(release < exit, "released before MethodExit");
    assert!(body[..release].contains("method_lock_parity_enabled(shared)"));
}

/// t1 wave 5, lane K (residual 1): HotSpot's early-return entry calls
/// `remove_activation(.., throw_monitor_exception = false,
/// install_monitor_exception = false, ..)`, so a forced return over a
/// method monitor the thread no longer owns -- refused, or released by the
/// frame's own `monitorexit` -- returns the forced value with no exception,
/// and leaves no exited bit behind.
#[test]
fn a_forced_early_return_over_an_unowned_method_monitor_returns_without_an_exception() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(305), "t1k-forced-unowned");
    let obj = shared.mem.heap.alloc_object(cratonvm_types::ClassId::new(7), 0);
    let mut f = frame(&mut thread);
    f.monitor_on_exit = Some(obj); // never entered: the release is refused
    thread.frames.push(f);
    let step = super::super::finish_forced_return(&shared, &mut thread, 0, 0, false);
    assert!(matches!(step, Ok(super::super::ForcedReturnStep::Returned(None))));
    assert!(thread.frames[0].monitor_on_exit.is_none());
    assert!(!thread.frames[0].held_monitors.method_monitor_exited());
    // The frame's own `monitorexit` released it earlier.
    thread.frames[0].held_monitors.set_method_monitor_exited();
    let step = super::super::finish_forced_return(&shared, &mut thread, 0, 0, false);
    assert!(matches!(step, Ok(super::super::ForcedReturnStep::Returned(None))));
    assert!(!thread.frames[0].held_monitors.method_monitor_exited());
}

/// t1 wave 5, lane K (residual 2): an OSR exit transfer rewrites a live
/// frame's record with `clear_entries`, which keeps the exited bit; frame
/// reuse (`clear`) drops it.
#[test]
fn a_rewritten_record_keeps_the_exited_bit_and_frame_reuse_drops_it() {
    let mut h = HeldMonitors::new();
    h.set_method_monitor_exited();
    h.clear_entries();
    assert!(h.method_monitor_exited());
    h.set_provenance(super::RecordProducer::OsrExitTransfer);
    assert!(h.method_monitor_exited(), "a later provenance mark keeps it");
    h.clear();
    assert!(!h.method_monitor_exited() && h.is_empty());
}

/// t1 wave 5, lane K (residual 3): a refused release in a BALANCED method
/// (javac-shaped; HotSpot can never get there) is a VM double release: the
/// `warn` line, and no Java IMSE replacing the program's own outcome. A
/// proven-unbalanced method (hand-written bytecode) keeps HotSpot's IMSE.
#[test]
fn a_refused_release_in_a_balanced_method_stays_a_warn_line() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(306), "t1k-balanced-refusal");
    let obj = shared.mem.heap.alloc_object(cratonvm_types::ClassId::new(7), 0);
    // `synchronized void m() { return; }`: no block monitor at all.
    let mut balanced = Frame::new_pooled(
        cratonvm_types::ClassId::new(0),
        Arc::from("T"),
        Arc::from("m"),
        Arc::from("()V"),
        None,
        Arc::from(&[0xb1u8][..]),
        Arc::from(Vec::new().into_boxed_slice()),
        4,
        4,
        &[],
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    balanced.monitor_on_exit = Some(obj); // never entered: refused
    thread.frames.push(balanced);
    assert!(release_method_monitor_at_return(&shared, &mut thread, 0).is_none());
    assert!(thread.frames[0].monitor_on_exit.is_none());
    assert!(!thread.frames[0].held_monitors.method_monitor_exited());
    // The hand-written shape: refused, and the IMSE stands.
    let mut unbalanced = frame(&mut thread);
    unbalanced.monitor_on_exit = Some(obj);
    thread.frames.push(unbalanced);
    assert!(release_method_monitor_at_return(&shared, &mut thread, 1).is_some());
}
