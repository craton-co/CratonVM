//! zr z11/nd: a restartable native whose one-attempt allocation door
//! refused is re-run by the funnel after a collection
//! (`native_oom_replay`), instead of raising `OutOfMemoryError` over a heap
//! full of garbage (`zgc-zr8nr-one-attempt-native-doors-remaining-20261013.md`).

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use std::cell::Cell;
use std::sync::atomic::Ordering;

thread_local! {
    static CALLS: Cell<u32> = const { Cell::new(0) };
    static SEEN: Cell<i32> = const { Cell::new(0) };
}

/// Allocates 1 MiB through the ONE-attempt door (the unwind on a refusal)
/// and records the int its argument array holds, read through the
/// argument it was handed.
fn one_attempt_mib(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    CALLS.with(|c| c.set(c.get() + 1));
    if let Some(Value::Object(Some(a))) = args.first() {
        if let Value::Int(v) = ctx.get_array_element(*a, 0) {
            SEEN.with(|s| s.set(v));
        }
    }
    let arr = ctx.new_array(ArrayElementType::Byte, 1 << 20);
    Ok(Some(Value::Object(Some(arr))))
}

/// A ZGC VM whose arena is full of UNROOTED 1 MiB arrays (garbage a
/// collection frees), with the funnel's pre-call relief latches lowered so
/// the first run of the native meets the full arena.
#[cfg(feature = "zgc")]
fn full_zgc_vm() -> Option<SharedVm> {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    });
    if !shared.mem.heap.is_zgc() {
        return None;
    }
    let mut served = 0usize;
    while shared
        .mem
        .heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 1 << 20)
        .is_some()
    {
        served += 1;
        assert!(served < 1 << 16, "the arena never refused");
    }
    shared.mem.heap.clear_young_spill_pressure();
    shared.mem.heap.clear_hard_alloc_failure();
    Some(shared)
}

fn reset(cb: NativeCallback) {
    CALLS.with(|c| c.set(0));
    SEEN.with(|s| s.set(0));
    TEST_OOM_RESTARTABLE.with(|c| c.set(cb as usize));
}

fn is_oome(r: &MethodCallResult) -> bool {
    matches!(
        r,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { .. }
        )))
    )
}

/// The defect and the fix: the first run refuses (the arena is full of
/// garbage); the funnel collects and runs the native again, which is
/// served. Before z11/nd the first refusal was the `OutOfMemoryError`.
#[test]
#[cfg(feature = "zgc")]
fn a_refused_restartable_native_is_rerun_after_a_collection() {
    let Some(shared) = full_zgc_vm() else {
        return;
    };
    let mut thread = JvmThread::new(ThreadId(1101), "zr11nd-replay");
    reset(one_attempt_mib);
    let out = safe_native_call(&shared, &mut thread, one_attempt_mib, &[]);
    assert!(matches!(out, Ok(Some(Value::Object(Some(_))))), "{out:?}");
    assert_eq!(
        CALLS.with(|c| c.get()),
        2,
        "one refused run, one served replay"
    );
    let census = &shared.mem.tlab_wedge.zgc_refusal;
    assert_eq!(census.native_replays.load(Ordering::Relaxed), 1);
    assert_eq!(census.native_replay_served.load(Ordering::Relaxed), 1);
    let line = crate::runtime::interpreter::gc_and_alloc::zgc_refusal_line(&shared)
        .expect("a ZGC line");
    assert!(
        line.contains(" native_replays=1 native_replay_served=1 "),
        "{line}"
    );
    assert!(
        thread.native_pin_roots.is_empty(),
        "the funnel releases every pin"
    );
}

/// First run: the one-attempt door (refuses on the full arena). Every
/// later run: a Java error of the native's own, as a body with its own
/// failing ladder answers.
fn refuses_then_answers_an_error(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let n = CALLS.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    if n == 1 {
        let arr = ctx.new_array(ArrayElementType::Byte, 1 << 20);
        return Ok(Some(Value::Object(Some(arr))));
    }
    Err(RuntimeError::OutOfMemoryError {
        message: "Java heap space (zs1nd test)".to_string(),
    }
    .into())
}

/// zs s1/nd: a re-run that answers a Java error is the answer, but it is
/// not "served": `native_replay_served` counts only re-runs that returned
/// a value (it used to count every `Ok`, including `Ok(Err(..))`).
#[test]
#[cfg(feature = "zgc")]
fn a_rerun_that_answers_an_error_is_not_counted_served() {
    let Some(shared) = full_zgc_vm() else {
        return;
    };
    let mut thread = JvmThread::new(ThreadId(1106), "zs1nd-served");
    reset(refuses_then_answers_an_error);
    let out = safe_native_call(&shared, &mut thread, refuses_then_answers_an_error, &[]);
    assert!(out.is_err(), "{out:?}");
    assert_eq!(CALLS.with(|c| c.get()), 2, "one refused run, one re-run");
    let census = &shared.mem.tlab_wedge.zgc_refusal;
    assert_eq!(census.native_replays.load(Ordering::Relaxed), 1);
    assert_eq!(census.native_replay_served.load(Ordering::Relaxed), 0);
    assert!(thread.native_pin_roots.is_empty());
}

/// The replay hands the native its argument re-read from the pin: the
/// collection may have moved it, and the second run still reads it.
#[test]
#[cfg(feature = "zgc")]
fn the_replay_rereads_its_arguments_from_their_pins() {
    let Some(shared) = full_zgc_vm() else {
        return;
    };
    // A live int[1] holding 42, made before the arena fills would be
    // ideal; make room for it by one refusal-collection first.
    let mut thread = JvmThread::new(ThreadId(1102), "zr11nd-args");
    crate::runtime::interpreter::maybe_gc_forced_pub_at(&shared, &mut thread, "vm-exec");
    let arg = shared
        .mem
        .heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Int, 1)
        .expect("room after a collection");
    {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.set_array_element(arg, 0, Value::Int(42));
    }
    while shared
        .mem
        .heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 1 << 20)
        .is_some()
    {}
    shared.mem.heap.clear_young_spill_pressure();
    shared.mem.heap.clear_hard_alloc_failure();
    reset(one_attempt_mib);
    let out = safe_native_call(
        &shared,
        &mut thread,
        one_attempt_mib,
        &[Value::Object(Some(arg))],
    );
    assert!(matches!(out, Ok(Some(Value::Object(Some(_))))), "{out:?}");
    assert_eq!(CALLS.with(|c| c.get()), 2);
    assert_eq!(SEEN.with(|s| s.get()), 42, "the replay read its argument");
}


/// A body nobody listed is never re-run (the positive control's twin:
/// the same native, not admitted).
#[test]
#[cfg(feature = "zgc")]
fn an_unlisted_native_is_not_rerun() {
    let Some(shared) = full_zgc_vm() else {
        return;
    };
    let mut thread = JvmThread::new(ThreadId(1104), "zr11nd-unlisted");
    reset(one_attempt_mib);
    TEST_OOM_RESTARTABLE.with(|c| c.set(0));
    let out = safe_native_call(&shared, &mut thread, one_attempt_mib, &[]);
    assert!(is_oome(&out), "{out:?}");
    assert_eq!(CALLS.with(|c| c.get()), 1);
}



/// gci i31/om: the collector rule reads only its own collector's switch.
#[test]
fn gci_i31om_collector_rule_reads_only_its_own_switch() {
    assert!(native_oom_replay_collector_admits(true, || true, || unreachable!()));
    assert!(!native_oom_replay_collector_admits(true, || false, || unreachable!()));
    assert!(native_oom_replay_collector_admits(false, || unreachable!(), || true));
    assert!(!native_oom_replay_collector_admits(false, || unreachable!(), || false));
}

/// ZGC admits under its own switch (zr z11/nd, unchanged by gci i31/om).
#[test]
#[cfg(feature = "zgc")]
fn zgc_replay_admission_is_unchanged() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        ..VmConfig::default()
    });
    if shared.mem.heap.is_zgc() {
        reset(one_attempt_mib);
        assert!(native_oom_replay_admits(&shared, one_attempt_mib));
        // A production body from the audited list is admitted too.
        assert!(native_oom_replay_admits(
            &shared,
            cratonvm_native_builtins::oom_restartable::restartable_bodies()[0]
        ));
    }
}
