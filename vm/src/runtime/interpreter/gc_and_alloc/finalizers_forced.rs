#[allow(unused_imports)]
use super::*;

/// `System.gc()` / `System.runFinalization()`'s drain: run pending finalizers
/// even while a JIT helper holds the thread borrow.
///
/// # Why the deferral had to go on this path
///
/// The guard in [`run_finalizers_impl`] says "defer to the next top-level
/// safepoint". For an application that only ever asks for finalization from
/// inside a compiled loop, **that safepoint never comes**: every
/// `System.gc()` arrives through a JIT helper, so `is_jit_thread_set()` is true
/// at every call and the drain is deferred forever.
///
/// MEASURED (`probes/ThreadRetainMin.java`, `CRATONVM_DBG_FINCAND=1`): the
/// objects are found dead and resurrected on EVERY cycle
/// (`dead_resurrected=1,2,3` some thirty times each) because
/// `finalizable_roots` re-adds the pending queue as roots to keep them alive
/// for a finalizer that never runs. `finalize()` never executes, the object is
/// never reclaimed, and a `WeakReference` to it never clears. Under `--nojit`
/// the same probe resurrects once and collects in two rounds.
///
/// That is `RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced`,
/// and it is not about Threads, netty, or JUnit: any object with a `finalize()`
/// override is immortal once the loop asking for the collection is compiled.
///
/// The re-entrancy the guard protects against is handled the way
/// [`run_cleaner_actions_forced`] already handles it for the sibling queue —
/// re-install the JIT thread pointer for the nested Java call under RAII, so an
/// unwinding `finalize()` cannot leave the outer level un-restored.
pub(in crate::runtime::interpreter) fn run_finalizers_forced(shared: &SharedVm, thread: &mut JvmThread) {
    // gci i35/jf (page `gci-i35jf-system-gc-runs-queued-finalizers-on-the-caller-FIXED-20261016`):
    // a collection door (`System.gc()`, the VM-internal and JVMTI forced
    // collections) leaves a busy delivery thread's queue to it, as HotSpot's
    // `System.gc()` never runs a finalizer on its caller. `runFinalization`
    // is not this door: [`run_pending_finalizers_for_runtime`] still drains.
    //
    // zt t18/x14: on ZGC a collection door waits for no finalizer (see
    // `system_gc_skips_finalizer_wait`).
    if system_gc_skips_finalizer_wait(shared) {
        run_finalizers_for_collection_door(shared, thread);
        return;
    }
    // zt t30/fz: on G1 / Generational a lock-free door hands the queue to the
    // delivery thread (`gc_finalizers_off_caller_all`).
    if collection_door_hands_off_finalizers(shared, thread) {
        return;
    }
    run_finalizers_forced_as(shared, thread, true);
}

/// zt t30/fz (`CRATONVM_GC_FINALIZERS_OFF_CALLER_ALL`, see
/// [`gc_finalizers_off_caller_all`]): [`run_finalizers_forced`] on G1 /
/// Generational for a caller holding no lock (a lock holder keeps
/// [`finalizers_handed_off_for_forced_drain`]'s hand-off) under the default
/// policy. The queue goes to the delivery thread and the caller waits for its
/// finalizer steps, as before it waited for its own inline drain -- bounded
/// by the finalizer timeout overall; what is left then stays with the
/// delivery thread, since HotSpot's `System.gc()` never runs a finalizer on
/// its caller. `false` (nothing queued, the switch off, or no delivery thread
/// could be started): the forced drain as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn collection_door_hands_off_finalizers(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    if !gc_finalizers_off_caller_all(shared)
        || finalizer_delivery_policy() != FinalizerDelivery::WhenLockHeld
        || thread_holds_user_locks(shared, thread)
    {
        return false;
    }
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(shared.mem.finalizer_thread.timeout_ms());
    !matches!(
        hand_off_finalizer_queue(shared, thread, Some(deadline)),
        FinalizerHandOff::NotHandedOff
    )
}

/// `CRATONVM_ZGC_SYSTEM_GC_SKIPS_FINALIZER_WAIT` (zt t18/x14, the x10 review
/// page's item 21e; default ON, `=0` kill). HotSpot 25's `System.gc()` is
/// `JVM_GC` -> one collection: the `Finalizer` thread runs what it queued,
/// asynchronously, and only `Runtime.runFinalization()` waits for finalizers.
/// This VM's collection doors waited twice: a lock-holding caller for the
/// delivery thread's batch (bounded by the finalizer timeout), and a lock-free
/// one, after its inline drain, for the delivery thread's finalizer step
/// (gc-common w5-d, narrowed to the step by zt t15/x12). A peer that keeps
/// making finalizable garbage keeps that step running, so every `System.gc()`
/// waited out the 2-s timeout. w5-d's promise (`System.gc();
/// System.runFinalization();` then a read of a finalizer's side effect) is
/// `runFinalization`'s to keep, and it still keeps it: that door drains and
/// waits exactly as before. With the switch a collection door hands a
/// lock-holder's finalizers to the delivery thread without waiting, and a
/// lock-free caller's inline drain (the pre-existing default policy shape)
/// runs at most the finalizers queued when it began, so a churning peer cannot
/// extend it either. ZGC only; `=0`: the waits, as before.
///
/// g1gen (item 10c of `g1-and-generational-items-carried-from-the-zgc-zt-round`,
/// rows `30fz_WI_*`: G1 / Serial `System.gc()` waited for a `finalize()` already
/// running on the delivery thread, `ended_when_gc_returned=true` where HotSpot
/// answers `false`): on G1 and Generational the same no-wait door under
/// [`gc_finalizers_off_caller_all`], which already hands the queue over.
pub(in crate::runtime::interpreter::gc_and_alloc) fn system_gc_skips_finalizer_wait(shared: &SharedVm) -> bool {
    if !shared.mem.heap.is_zgc() {
        return gc_finalizers_off_caller_all(shared);
    }
    true
}

/// [`run_finalizers_forced`] under [`system_gc_skips_finalizer_wait`]: the
/// hand-off of [`finalizers_handed_off_for_forced_drain`] without its wait,
/// and the inline drain bounded by the queue length at its start, with no
/// [`wait_out_in_flight_finalizer_batch`]. A hand-off that finds no delivery
/// thread (none could be started) drains inline, as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_finalizers_for_collection_door(shared: &SharedVm, thread: &mut JvmThread) {
    let policy = finalizer_delivery_policy();
    let ft = &shared.mem.finalizer_thread;
    let on_delivery_thread = ft.delivery_thread() == Some(thread.thread_id.0);
    let handed_off = !on_delivery_thread
        && match policy {
            FinalizerDelivery::Inline => false,
            FinalizerDelivery::Always => wake_finalizer_thread(shared),
            FinalizerDelivery::WhenLockHeld => {
                ft.pending_count() > 0
                    && (finalizers_off_caller(shared) || thread_holds_user_locks(shared, thread))
                    && request_finalizer_thread(shared, true)
            }
        };
    if handed_off {
        return;
    }
    let queued = ft.pending_count();
    if queued > 0 {
        run_finalizers_forced_inline(shared, thread, queued);
    }
}



/// [`run_finalizers_forced`]'s drain; `leave_busy` is
/// [`forced_drain_left_to_delivery`]'s.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_finalizers_forced_as(shared: &SharedVm, thread: &mut JvmThread, leave_busy: bool) {
    // gc-common w3-d / w4-d: where the delivery thread takes `finalize()` (see
    // `FinalizerDelivery`), `System.gc()` keeps its "finalizers have run when I
    // return" ordering by waiting (bounded) for it. Unserved, a caller holding
    // no lock falls through to the inline drain (unless `leave_busy`, gci
    // i35/jf); a caller holding one returns and leaves the queue to the
    // delivery thread (JLS §12.6).
    if finalizers_handed_off_for_forced_drain(shared, thread, leave_busy) {
        return;
    }
    run_finalizers_forced_inline(shared, thread, usize::MAX);
    // gc-common w5-d: and do not return while the delivery thread is still
    // running a finalizer it dequeued before this drain looked.
    wait_out_in_flight_finalizer_batch(shared, thread);
}

/// Under the default policy ([`FinalizerDelivery::WhenLockHeld`]) a lock-free
/// forced drain (`System.gc()`, `runFinalization()`) runs inline — but once a
/// delivery thread exists, its batch drains the same queue. An object that batch
/// dequeued just before the inline drain looked is not run here, and the caller
/// returned while its `finalize()` was still running on the delivery thread:
/// `System.gc(); System.runFinalization();` followed by a read of the
/// finalizer's side effect could see it missing.
///
/// gc-common w5-d (2026-09-24). Pre-existing under `--compatible` (the thread
/// starts there the first time a lock-holding thread has finalizers pending),
/// and far more reachable once a `--real-jdk` VM starts the thread for its queue
/// wake-ups (`queue_wake_ups_recorded_for`). Waits only while a batch is owed or
/// running, bounded by the finalizer timeout and sliced like every other forced
/// wait; never on the delivery thread itself, and not under the full hand-off,
/// whose forced drain already waited for the thread before falling back here.
pub(in crate::runtime::interpreter::gc_and_alloc) fn wait_out_in_flight_finalizer_batch(shared: &SharedVm, thread: &mut JvmThread) {
    if finalizer_delivery_policy() != FinalizerDelivery::WhenLockHeld {
        return;
    }
    let ft = &shared.mem.finalizer_thread;
    if !ft.delivery_thread_claimed() || ft.delivery_thread() == Some(thread.thread_id.0) {
        return;
    }
    if !ft.delivery_signal().in_flight() {
        return;
    }
    let holds = thread_holds_user_locks(shared, thread);
    // zt t15/x12: on ZGC, the batch's finalizer step only (see
    // `system_gc_waits_finalizer_step_only`).
    if system_gc_waits_finalizer_step_only(shared) {
        let _ = wait_for_delivery_step(shared, thread, holds, DeliveryStep::Finalizers);
        return;
    }
    let _ = wait_for_delivery(shared, thread, holds);
}

/// `CRATONVM_ZGC_SYSTEM_GC_WAITS_FINALIZER_STEP_ONLY` (zt t15/x12, the x10
/// review page's missed-drain item; default ON, `=0` kill). w5-d's wait above
/// keeps one promise: a `finalize()` the delivery thread dequeued before this
/// caller's inline drain looked has run when the caller returns. It waited
/// for the WHOLE batch to finish, and under the default policy a batch ends
/// with the GC-notification step, which any other thread's collection feeds
/// (ZGC's hand-offs request a batch per collection, zt t12/gn4). With an
/// allocating peer, every `System.gc()` waited out the 2-s finalizer timeout:
/// `Zt13X10MissedDrain` (row `x10_N1c`, both modes) spent ~2.7 s per round,
/// 127.6 s CPU against HotSpot's 9.2 s, while the peer's notifications
/// overflowed the queue and pushed out the `System.gc()` ones (`late=8`).
/// HotSpot's `System.gc()` waits for no notification at all. With the switch
/// the caller waits only while the batch runs finalizers it dequeued
/// ([`cratonvm_gc::reference::DeliverySignal::begin_finalizer_step`]); the
/// promise is kept, since only that step dequeues finalizable objects. ZGC
/// only (G1 and Generational keep the batch wait); `=0`: the batch wait, as
/// before.
///
/// g1gen (item 10c, rows `30fz_WR_*`): on G1 and Generational the same step
/// wait under [`gc_finalizers_off_caller_all`], so a forced wait is not held
/// by the batch's later steps.
pub(in crate::runtime::interpreter::gc_and_alloc) fn system_gc_waits_finalizer_step_only(shared: &SharedVm) -> bool {
    if !shared.mem.heap.is_zgc() {
        return gc_finalizers_off_caller_all(shared);
    }
    true
}


/// The inline half of [`run_finalizers_forced`]; runs at most `limit`
/// finalizers (zt t18/x14: a collection door's bound, `usize::MAX` elsewhere).
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_finalizers_forced_inline(shared: &SharedVm, thread: &mut JvmThread, limit: usize) {
    if !forced_finalizers_enabled() {
        run_finalizers_impl_upto(shared, thread, false, limit);
        return;
    }
    // RAII so an unwinding finalizer cannot leave the outer JIT level
    // un-restored. Same shape as `run_cleaner_actions_forced`.
    struct NestedJitScope(Option<crate::jit::helpers::JitThreadScope>);
    impl Drop for NestedJitScope {
        fn drop(&mut self) {
            if let Some(scope) = self.0.take() {
                crate::jit::helpers::restore_jit_thread(scope);
            }
        }
    }
    let _scope = NestedJitScope(if crate::jit::helpers::is_jit_thread_set() {
        Some(crate::jit::helpers::set_jit_thread(thread))
    } else {
        None
    });
    run_finalizers_impl_upto(shared, thread, true, limit);
}

/// `Runtime.runFinalization()` / `System.runFinalization()`: run every
/// finalizer queued so far before returning — the forced drain `System.gc()`
/// ends with, without the collection.
///
/// gc-common w4-d (2026-09-23), `common-w3d-run-finalization-never-drains-the-vm-queue`.
/// This VM finalizes through its own queue (`Finalizer.register` is
/// intercepted, so the JDK's `Finalizer` queue is always empty), and neither
/// `runFinalization` route looked at it: the `compatible` natives drained a
/// process-global tracker nothing fed, and the `--real-jdk` bytecode forked a
/// secondary finalizer over the empty JDK queue. Finalizers an allocation-driven
/// collection had queued but not run (its drain declines under a JIT borrow,
/// and the allocation-failure door runs none) therefore waited for some later
/// door. Where the delivery thread serves (`FinalizerDelivery`), this waits for
/// it exactly as `System.gc()` does; HotSpot's `runFinalization` likewise
/// joins a secondary finalizer. Cleaner actions are not part of the contract
/// and stay on their own schedule.
pub(crate) fn run_pending_finalizers_for_runtime(shared: &SharedVm, thread: &mut JvmThread) {
    // zt t28/rf: on ZGC the queue runs off the caller, as HotSpot's
    // secondary finalizer runs it.
    if run_finalization_handed_off(shared, thread) {
        return;
    }
    // gci i35/jf: never `leave_busy` -- `runFinalization` promises the queue
    // ran (HotSpot's secondary finalizer runs it while `Finalizer` is busy).
    run_finalizers_forced_as(shared, thread, false);
}

/// `CRATONVM_ZGC_RUN_FINALIZATION_OFF_CALLER` (zt t28/rf, page
/// `zgc-zt27fin1-finalizable-churn-and-signals-CLOSED-20261022.md` item 3a; default
/// ON, `=0` kill; ZGC only, both modes, as its sibling
/// [`zgc_finalizers_off_caller`]). HotSpot 25's `Runtime.runFinalization()`
/// forks a "Secondary finalizer" thread and joins it, so no `finalize()` runs
/// on the caller; this VM drained a lock-free caller's queue inline. With the
/// switch the queue is handed to the delivery thread and the caller waits for
/// its finalizer step (the promise `runFinalization` makes; not the batch's
/// later steps, [`system_gc_waits_finalizer_step_only`]'s reason). A delivery
/// thread that cannot be had, or a wait that ends unserved (the finalizer
/// timeout, or the thread blocked on a monitor the caller holds), falls back
/// to the forced drain as before.
///
/// zt t30/fz: on G1 / Generational the same under
/// `CRATONVM_GC_FINALIZERS_OFF_CALLER_ALL` ([`gc_finalizers_off_caller_all`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_finalization_off_caller(shared: &SharedVm) -> bool {
    if !shared.mem.heap.is_zgc() {
        return gc_finalizers_off_caller_all(shared);
    }
    true
}



/// zt t28/rf: how many served finalizer steps in a row may leave the queue
/// as long before [`run_finalization_handed_off`] gives the rest to the
/// caller's drain.
pub(in crate::runtime::interpreter::gc_and_alloc) const RUN_FINALIZATION_STALLED_STEPS: u32 = 3;

/// [`run_pending_finalizers_for_runtime`] under
/// [`run_finalization_off_caller`]: `true` when the delivery thread ran the
/// queue (the caller returns), `false` when the caller must drain as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_finalization_handed_off(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    if !run_finalization_off_caller(shared) {
        return false;
    }
    matches!(
        hand_off_finalizer_queue(shared, thread, None),
        FinalizerHandOff::Ran
    )
}

/// What [`hand_off_finalizer_queue`] did. zt t30/fz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::gc_and_alloc) enum FinalizerHandOff {
    /// Nothing was handed over (empty queue, the delivery thread asking, the
    /// `Inline` policy, or no delivery thread could be started).
    NotHandedOff,
    /// The delivery thread emptied the queue.
    Ran,
    /// The queue was handed over but the wait gave up (no progress, the
    /// finalizer timeout, the thread blocked on the caller's monitor, or the
    /// deadline): the queue is the delivery thread's; the caller decides
    /// whether to drain the rest.
    GaveUp,
}

/// Hand the finalizer queue to the delivery thread and wait for its
/// finalizer steps until the queue is empty or `deadline` passes (zt t28/rf's
/// loop; zt t30/fz shares it with the G1 / Generational collection door).
pub(in crate::runtime::interpreter::gc_and_alloc) fn hand_off_finalizer_queue(
    shared: &SharedVm,
    thread: &mut JvmThread,
    deadline: Option<std::time::Instant>,
) -> FinalizerHandOff {
    if finalizer_delivery_policy() == FinalizerDelivery::Inline {
        return FinalizerHandOff::NotHandedOff;
    }
    let ft = &shared.mem.finalizer_thread;
    if ft.delivery_thread() == Some(thread.thread_id.0) || ft.pending_count() == 0 {
        return FinalizerHandOff::NotHandedOff;
    }
    let holds = thread_holds_user_locks(shared, thread);
    // zt t30/fz: off ZGC a wait after one that saw no progress waits for a
    // whole batch begun after the request (`Batch`), not for the finalizer
    // step: a thread that has not begun its step yet answers the step wait
    // at once, and three such answers in a row gave the queue back to the
    // caller. ZGC keeps t28's step waits.
    //
    // zt t31/x10 (x10 page, row `30fz_rf_zgcc`, `CRATONVM_ZGC_RUN_FINALIZATION_STALL_WAITS_BATCH`,
    // default ON, `=0` kill): ZGC too. Under `--compatible` its `System.gc()`
    // door hands the GC notifications to the delivery thread, which delivers
    // them BETWEEN finalizer slices; a step wait that lands there is served
    // at once with the queue untouched, three such serves gave the queue
    // back, and `runFinalization()` ran every `finalize()` on its caller (1
    // run in 5 on the m9 / m11 hosts; HotSpot: never).
    let stall_step = if shared.mem.heap.is_zgc()
        && false
    {
        DeliveryStep::Finalizers
    } else {
        DeliveryStep::Batch
    };
    // A served step may leave part of the queue to the next one (the
    // delivery thread runs finalizers in slices, `delivery_slices_finalizers`),
    // and the first one served may be the step already in flight (row
    // `rf28_Dc_3`: a slow `finalize()` the thread was running; the queue had
    // not moved yet): ask again while the queue shrinks, and give up -- the
    // rest to the caller's drain -- only after
    // [`RUN_FINALIZATION_STALLED_STEPS`] served steps in a row that did not
    // shorten it.
    let mut before = usize::MAX;
    let mut stalled = 0;
    let mut requested = false;
    loop {
        let pending = ft.pending_count();
        if pending == 0 {
            return FinalizerHandOff::Ran;
        }
        let progressed = pending < before;
        if progressed {
            stalled = 0;
        } else {
            stalled += 1;
            if stalled >= RUN_FINALIZATION_STALLED_STEPS {
                return FinalizerHandOff::GaveUp;
            }
        }
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return FinalizerHandOff::GaveUp;
        }
        if !request_finalizer_thread(shared, true) {
            return if requested {
                FinalizerHandOff::GaveUp
            } else {
                FinalizerHandOff::NotHandedOff
            };
        }
        requested = true;
        before = pending;
        let step = if progressed {
            DeliveryStep::Finalizers
        } else {
            stall_step
        };
        let wait = wait_for_delivery_step(shared, thread, holds, step);
        if !matches!(wait, DeliveryWait::Served) {
            return FinalizerHandOff::GaveUp;
        }
    }
}

/// `JavaLangRefAccess.waitForReferenceProcessing()`: if the reference-delivery
/// thread has cleaner or finalizer work queued or in flight for this VM, wait
/// for that batch (bounded, in a blocking region) and answer `true`; otherwise
/// answer `false` at once. HotSpot's answer means the same thing: "the
/// Reference Handler had work, and I waited for it".
///
/// gc-common w5-d (2026-09-24),
/// `common-w4d-wait-for-reference-processing-ignores-the-delivery-thread`. The
/// bridge answered `false` unconditionally ("reference processing runs inline
/// with GC"), which the full hand-off made untrue: an allocation-driven
/// collection queues `DirectByteBuffer`'s `Deallocator` and returns, and the
/// delivery thread frees the memory later. `java.nio.Bits.reserveMemory` asks
/// this before it falls back to `System.gc()` — a FULL collection — and its
/// sleep ladder, so a `false` while the memory was about to be freed cost one
/// full GC per reservation that missed.
///
/// Only under the full hand-off (`CRATONVM_FINALIZER_THREAD=1`): under the
/// default policy the delivery thread runs nothing but the finalizers of
/// lock-holding threads, which `Bits` does not wait for, and cleaner actions
/// stay on their inline doors — so `--compatible` answers exactly as before.
/// The delivery thread asking about itself answers `false` (it would wait on
/// its own batch). A wait that ends unserved — the finalizer timeout, or the
/// delivery thread blocked on a monitor the caller holds — answers `false`, so
/// `Bits`' `while (refprocActive)` loop cannot spin on a stuck batch.
pub(crate) fn wait_for_reference_processing_for_runtime(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> bool {
    if !finalizer_thread_enabled() {
        return false;
    }
    let ft = &shared.mem.finalizer_thread;
    if ft.delivery_thread() == Some(thread.thread_id.0) {
        return false;
    }
    let queued = finalizer_thread_has_work(shared);
    // `in_flight` only while a thread is claimed: a spawn that failed hands the
    // claim back but leaves its pre-spawn request owed forever.
    let in_flight = ft.delivery_thread_claimed() && ft.delivery_signal().in_flight();
    if !queued && !in_flight {
        return false;
    }
    // Queued work the thread may not have been asked for yet (a door that
    // deferred it): ask, starting the thread on first use. Work only in flight
    // is already covered by the ticket `wait_for_delivery` takes.
    if queued && !request_finalizer_thread(shared, true) {
        return false;
    }
    let holds = thread_holds_user_locks(shared, thread);
    wait_for_delivery(shared, thread, holds) == DeliveryWait::Served
}

/// Run every queued finalizer; answers how many were dequeued (zt t27/fin1).
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_finalizers_impl(shared: &SharedVm, thread: &mut JvmThread, forced: bool) -> usize {
    run_finalizers_impl_upto(shared, thread, forced, usize::MAX)
}

/// [`run_finalizers_impl`] that stops after `limit` dequeued objects (zt
/// t18/x14, [`run_finalizers_for_collection_door`]). Answers how many objects
/// it dequeued (zt t27/fin1, [`run_delivery_finalizer_slice`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_finalizers_impl_upto(
    shared: &SharedVm,
    thread: &mut JvmThread,
    forced: bool,
    limit: usize,
) -> usize {
    // bc math-ec 0x4 exclusion switches — see `process_references_after_gc`.
    if no_refproc() || no_cleaners() {
        return 0;
    }
    // Same JIT-borrow re-entrancy guard as `run_cleaner_actions`: a `finalize()`
    // invoked while a JIT helper holds the `&mut JvmThread` could re-enter the
    // JIT and alias the borrow. Defer to the next top-level safepoint; the
    // queue is GC-relocated via `FinalizerThread::update_after_gc`.
    //
    // `forced` callers have re-installed the JIT thread pointer around this
    // call, so the nested invocation cannot alias the borrow.
    if !forced && crate::jit::helpers::is_jit_thread_set() {
        return 0;
    }
    let mut ran = 0usize;
    loop {
        if ran >= limit {
            break;
        }
        ran += 1;
        let obj_addr = match shared.mem.finalizer_thread.dequeue() {
            Some(addr) => addr,
            None => {
                // Nothing dequeued on this turn: not counted.
                ran -= 1;
                break;
            }
        };
        // SAFETY: obj_addr was produced by the finalizer thread's dequeue and points at a valid object header within the heap arena.
        let obj_ref = unsafe { ObjectRef::from_raw(obj_addr as *mut u8) };
        let class_id = shared.mem.heap.class_id_of(obj_ref);

        let class_known = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_some();

        if class_known {
            // JIT round 21 wave 22, lane finalize
            // (`VmConfig::jni_reinit_registers_finalizer_again`): a JNI
            // re-`<init>` gave this object a second HotSpot `Finalizer`
            // (`ReferenceEntry::extra_finalizers`), so `finalize()` runs once
            // more. The object is queued again BEFORE this run, while
            // `obj_addr` is current: the queue roots and relocates it across
            // whatever this `finalize()` allocates, and the loop below (or the
            // next drain) runs it from there.
            if shared.config.jni_reinit_registers_finalizer_again
                && shared.mem.ref_processor.lock().take_extra_finalizer(obj_addr)
            {
                // Wave 24 (item 8): an accepted re-queue is counted, so a
                // refused one (the run would be lost) is visible.
                if shared.mem.finalizer_thread.enqueue_unfinalized(obj_addr) {
                    shared.mem.finalizer_thread.note_extra_run_requeued();
                }
            }
            // Invoke finalize()V — errors are silently swallowed per JLS §12.6.
            //
            // gc-common w1-d (2026-09-23): dispatched on the object's own
            // `ClassId`, not re-resolved from its NAME. `invoke_shared` looks
            // the name up loader-blind (`load_class_concurrent`), so for a class
            // defined by a user loader it either reports "ambiguous" the moment
            // a second loader defines the same name — every redeploy of the same
            // webapp, every forked test class loader — or resolves (or LOADS)
            // the application-classpath class of that name instead; either way
            // `finalize()` silently did not run on the object it was for, or ran
            // another class's body against it. The class id in the header is
            // exact; `invoke_by_class_id_shared` exists for this.
            let _ = crate::vm::invoke_by_class_id_shared(
                shared,
                thread,
                class_id,
                "finalize",
                "()V",
                &[Value::Object(Some(obj_ref))],
            );
        }
    }
    // zt t31/x10 (`CRATONVM_DBG_FINALIZER_DELIVERY=1`, reads only): name
    // every drain that ran `finalize()` OFF the delivery thread, with the
    // Rust stack that chose it (row `30fz_rf_zgcc`'s rare caller run).
    if ran > 0
        && finalizer_delivery_census_on()
        && shared.mem.finalizer_thread.delivery_thread() != Some(thread.thread_id.0)
    {
        eprintln!(
            "[fin-inline] thread={} ran={ran} forced={forced} left={} claimed={}\n{}",
            thread.thread_id.0,
            shared.mem.finalizer_thread.pending_count(),
            shared.mem.finalizer_thread.delivery_thread_claimed(),
            std::backtrace::Backtrace::force_capture()
        );
    }
    ran
}

// ---------------------------------------------------------------------------
// The reference-delivery thread ("Craton Finalizer") — gc-common w3-d
// ---------------------------------------------------------------------------
//
// `common-w2f-finalizers-and-cleaners-run-on-the-allocating-mutator-FIXED-20260923.md`:
// `finalize()`, cleaner actions and `DirectByteBuffer` deallocators ran inline
// on whichever mutator's allocation triggered the collection — in the middle
// of an arbitrary `new`, with whatever monitors (re-entrant, so a finalizer
// could ENTER the allocating thread's half-finished critical section),
// `ThreadLocal`s, context class loader and interrupt status that thread had.
// JLS §12.6: the thread that invokes a finalizer holds no user-visible locks.
//
// With `CRATONVM_FINALIZER_THREAD=1` a per-VM daemon thread does that work
// instead, and also the `ReferenceQueue` wake-ups the collector owes
// (`docs/feature-designs/real-jdk-mode.md`):
//
// * every collection door still QUEUES exactly what it queued before (the
//   `FinalizerThread` / `CleanerThread` queues, rooted and relocated as
//   before) and now bumps the delivery signal;
// * the allocation doors (`run_finalizers` / `run_cleaner_actions`) run
//   nothing;
// * `System.gc()` (`run_*_forced`) waits, in a blocking region and for at most
//   the finalizer timeout, until the delivery thread has served everything
//   queued before it asked — so `System.gc(); System.runFinalization();`
//   ordering is kept — and drains inline only if that wait times out;
// * the delivery thread, idle, is GC-blocked (never counted by a pause) and
//   holds only a weak handle to its VM.
//
// It is also the drain that `docs/feature-designs/real-jdk-mode.md`
// lacked: the allocation-FAILURE door drains nothing, but every door's pause
// ends in `process_references_after_gc` / `enqueue_resurrected_finalizers`,
// which wake this thread (and start it on first use).
//
// DEFAULT (gc-common w4-d, 2026-09-23): the delivery thread is used for
// `finalize()` exactly where running it inline would break JLS §12.6 — when
// the thread that would run it holds a user-visible lock (a monitor, or an
// `AbstractOwnableSynchronizer`), see [`FinalizerDelivery::WhenLockHeld`].
// Everything else — cleaner actions, queue wake-ups, a lock-free mutator's
// finalizers — keeps its pre-w3 inline path, so `--compatible` output is
// unchanged except for the §12.6 case. `CRATONVM_FINALIZER_THREAD=1` still
// selects the full hand-off (every door, cleaners and wake-ups included);
// `=0` restores the inline drain everywhere (bisection).
//
// gc-common w5-d (2026-09-24): a `--real-jdk` VM also hands its `ReferenceQueue`
// wake-ups to the delivery thread under the default policy
// (`queue_wake_ups_recorded_for`). Where the `ReferenceQueue` bridges yield to
// the real `remove()` bytecode, that bytecode is `lock.wait()` and nothing else
// wakes it. The policy is read per VM from `SharedVm`, so a `--compatible` VM in
// the same process is untouched.
//
// GC notifications (JMX listeners): off the mutator for a `--real-jdk` VM
// (w6-d), and for a `--compatible` one exactly when the collecting thread holds
// a user-visible lock (w7-d) — the §12.6 line, drawn for listeners. See
// `gc_notifications_go_to_delivery_thread`.

/// How `finalize()` (and, under [`Self::Always`], cleaner actions and queue
/// wake-ups) is delivered. gc-common w4-d.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::gc_and_alloc) enum FinalizerDelivery {
    /// `CRATONVM_FINALIZER_THREAD=0`: the pre-w3 behaviour everywhere — every
    /// drain runs inline on the thread that reaches it.
    Inline,
    /// Unset (the default): inline, EXCEPT that a thread holding a
    /// user-visible lock never runs `finalize()` itself — it hands the queue
    /// to the delivery thread (started on first such use) and, on the forced
    /// paths, waits for it (bounded). JLS §12.6: "the thread that invokes the
    /// finalizer will not be holding any user-visible synchronization locks".
    /// A finalizer that synchronizes on a monitor the allocating thread holds
    /// used to ENTER it (monitors are re-entrant) half-way through that
    /// thread's critical section. Cleaner actions are not covered (their
    /// contract is the Cleaner's, not the JLS's) and keep the inline path.
    WhenLockHeld,
    /// `CRATONVM_FINALIZER_THREAD=1` (`CRATONVM_GC=finalizer-thread`): every
    /// door hands everything — finalizers, cleaner actions, `ReferenceQueue`
    /// wake-ups, GC notifications — to the delivery thread (gc-common w3-d).
    Always,
}

/// The delivery thread's registry / OS thread name.
pub(in crate::runtime::interpreter::gc_and_alloc) const FINALIZER_THREAD_NAME: &str = "Craton Finalizer";

/// How long the idle delivery thread sleeps between checks that its VM still
/// exists. A request wakes it at once; this bounds only how long a dropped VM
/// keeps the (then unregistered) OS thread around.
pub(in crate::runtime::interpreter::gc_and_alloc) const FINALIZER_THREAD_IDLE_TICK: std::time::Duration = std::time::Duration::from_secs(2);

/// `CRATONVM_FINALIZER_THREAD`, latched like every flag. See
/// [`FinalizerDelivery`] and [`finalizer_delivery_from`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizer_delivery_policy() -> FinalizerDelivery {
    static POLICY: std::sync::OnceLock<FinalizerDelivery> = std::sync::OnceLock::new();
    *POLICY.get_or_init(|| {
        finalizer_delivery_from(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()
                .as_deref(),
        )
    })
}

/// The value rule of [`finalizer_delivery_policy`]: unset is
/// [`FinalizerDelivery::WhenLockHeld`]; an explicit `0` / `false` / `off` /
/// `no` is [`FinalizerDelivery::Inline`]; any other value (the grouped
/// `CRATONVM_GC=finalizer-thread` spelling included) is
/// [`FinalizerDelivery::Always`] — the presence rule the other GC levers use,
/// with the off words now meaning "the pre-w3 inline drain".
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizer_delivery_from(value: Option<&str>) -> FinalizerDelivery {
    match value.map(str::trim) {
        None => FinalizerDelivery::WhenLockHeld,
        Some("0" | "false" | "off" | "no") => FinalizerDelivery::Inline,
        Some(_) => FinalizerDelivery::Always,
    }
}

/// The full hand-off (`CRATONVM_FINALIZER_THREAD=1`): every door hands every
/// queue to the delivery thread. The reference passes' in-pause wakes and the
/// queue wake-up recording are gated on this, not on the default policy.
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizer_thread_enabled() -> bool {
    finalizer_delivery_policy() == FinalizerDelivery::Always
}

/// Whether this VM's reference passes record the `ReferenceQueue`s they link
/// into, so the delivery thread can `notifyAll` each queue's `lock` after the
/// pause (`gc_enqueued_queues_notify`). See [`queue_wake_ups_recorded_for`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn queue_wake_ups_recorded(shared: &SharedVm) -> bool {
    // `aligned` answers `true` under `--real-jdk` for every row, so this is
    // the old `is_jdk_only()` plus `--compatible` over a real JDK with the
    // `JdkReferences` row (nio20, lane L4), where the `ReferenceQueue.remove`
    // stand-ins are not registered and the JDK's `lock.wait()` body runs.
    queue_wake_ups_recorded_for(
        finalizer_delivery_policy(),
        shared
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkReferences),
    )
}

/// The rule of [`queue_wake_ups_recorded`]: under the full hand-off always;
/// otherwise exactly when THIS VM runs the JDK's `ReferenceQueue.remove`
/// bodies (`--real-jdk`, or the `JdkReferences` row; never for
/// [`FinalizerDelivery::Inline`], the bisection arm).
///
/// gc-common w5-d (2026-09-24), `common-w2f-gc-enqueue-never-wakes-queue-waiters`.
/// Wherever the real `ReferenceQueue.remove()` bytecode runs, it is
/// `lock.wait()`, and the collector's splice notifies nobody. An untimed
/// `remove()` then waited for some OTHER `enqueue()` on the same queue —
/// possibly forever — and the Common Cleaner learned of a GC-enqueued
/// `PhantomCleanable` only at its 60-s timeout. Under `--real-jdk` that
/// bytecode runs as soon as the `Bridge` natives yield to it: today under the
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` dial (measured: both waiters asleep after
/// 10 s on the w4 binary), and for good once those bridges are retired. Waves
/// 3-4 fixed it only with `CRATONVM_FINALIZER_THREAD=1`. The strict policy is per VM
/// (`VmConfig::compatibility_mode`, read from `SharedVm`), never a process
/// flag, so one process may host a `--compatible` VM next to a strict one.
/// Under `--compatible` the natives poll (`rq_remove_wait`) and nothing is
/// recorded, so that mode is unchanged byte for byte.
pub(in crate::runtime::interpreter::gc_and_alloc) fn queue_wake_ups_recorded_for(policy: FinalizerDelivery, jdk_only: bool) -> bool {
    match policy {
        FinalizerDelivery::Always => true,
        FinalizerDelivery::WhenLockHeld => jdk_only,
        FinalizerDelivery::Inline => false,
    }
}

/// Whether a `ReferenceQueue.remove` waiter in this VM may sleep on the
/// queue's `lock` rather than poll: the reference passes record every queue
/// they link into ([`queue_wake_ups_recorded`]) AND a delivery thread has been
/// claimed to deliver them — [`gc_enqueued_queues_hand_off`] then always finds
/// a taker, so every recorded list is followed by
/// [`gc_enqueued_queues_notify`]'s `synchronized (lock) { lock.notifyAll(); }`.
/// Before the first claim (no collection has linked a `Reference` into a queue
/// yet, or no thread could be started) the answer is `false` and the waiter
/// keeps the poll. The `NativeSystemAccess::reference_queue_wake_ups_delivered`
/// hook; the waiter is `rq_remove_wait` (`native-builtins/src/reference.rs`),
/// which asks only under `--real-jdk`. gc-common w6-d (2026-09-24),
/// `common-w3d-proposal-compatible-queue-remove-waits-on-lock-RETIRED-20260927`.
pub(crate) fn queue_wake_up_delivery_serves(shared: &SharedVm) -> bool {
    queue_wake_ups_recorded(shared) && shared.mem.finalizer_thread.delivery_thread_claimed()
}

/// Anything queued for the delivery thread.
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizer_thread_has_work(shared: &SharedVm) -> bool {
    shared.mem.finalizer_thread.pending_count() > 0 || shared.mem.cleaner_thread.has_pending_work()
}

/// Whether `thread` holds a user-visible synchronization lock (JLS §12.6): a
/// monitor, or an `AbstractOwnableSynchronizer` (`ReentrantLock`,
/// `ReentrantReadWriteLock`'s write lock, …).
///
/// Read from the per-thread lists every `monitorenter` route and every AQS
/// ownership transition already maintain for `ThreadMXBean.getLockedMonitors()`
/// / `getLockedSynchronizers()` (`ThreadRegistry::jmx_lock_snapshot`). A miss
/// (a lock some route forgot to publish) answers `false`, i.e. the pre-w4
/// inline drain — never worse than before. Costs a registry read and two small
/// clones, so callers ask only when there is a finalizer to run.
pub(in crate::runtime::interpreter::gc_and_alloc) fn thread_holds_user_locks(shared: &SharedVm, thread: &JvmThread) -> bool {
    shared
        .threads
        .thread_registry
        .jmx_lock_snapshot(thread.thread_id)
        .is_some_and(|(_, _, monitors, synchronizers, ..)| {
            !monitors.is_empty() || !synchronizers.is_empty()
        })
}

/// Hand what is queued to the delivery thread, starting it on first use.
///
/// Returns `true` when a delivery thread serves (or is starting to serve) this
/// VM, i.e. the caller must NOT drain the queues itself; `false` when the full
/// hand-off is off or no thread could be started, and the caller keeps the
/// inline drain.
///
/// Callable from inside a pause (the reference passes call it): the request is
/// a counter bump and a condvar notify, and a first-use spawn only registers a
/// STARTING thread (`register_starting_with_daemon`, not counted by any census)
/// and spawns the OS thread, which cannot become a mutator until the pause has
/// completed — the same protocol a `Thread.start()` child racing a pause uses.
/// Nothing here touches the heap.
pub(in crate::runtime::interpreter::gc_and_alloc) fn wake_finalizer_thread(shared: &SharedVm) -> bool {
    if !finalizer_thread_enabled() {
        return false;
    }
    request_finalizer_thread(shared, finalizer_thread_has_work(shared))
}

/// [`wake_finalizer_thread`] without the policy gate: request a batch from
/// the delivery thread, starting it when none was ever claimed AND
/// `start_if_idle` says there is work worth a thread. `true` when a delivery
/// thread serves (or is starting to serve) the request.
pub(in crate::runtime::interpreter::gc_and_alloc) fn request_finalizer_thread(shared: &SharedVm, start_if_idle: bool) -> bool {
    let ft = &shared.mem.finalizer_thread;
    if ft.delivery_thread_claimed() {
        // Serving, or starting: its first batch drains whatever was queued
        // before it could serve, and this bump makes sure that batch covers
        // our work too.
        ft.request_delivery();
        return true;
    }
    if !start_if_idle {
        // Nothing to run and nothing to start a thread for yet.
        return false;
    }
    start_finalizer_thread(shared)
}

/// The allocation doors' question for CLEANER actions (`run_cleaner_actions`):
/// does the delivery thread take this batch? Full hand-off only. On the
/// delivery thread itself (an action allocated and collected) the answer is
/// also `true` — its own loop drains the batch the nested collection
/// requested, once the action that triggered it has returned.
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizer_thread_takes_delivery(shared: &SharedVm, thread: &JvmThread) -> bool {
    if !finalizer_thread_enabled() {
        return false;
    }
    if shared.mem.finalizer_thread.delivery_thread() == Some(thread.thread_id.0) {
        return true;
    }
    wake_finalizer_thread(shared)
}

/// The allocation doors' question for `finalize()` (`run_finalizers`): does
/// the delivery thread take it? Under [`FinalizerDelivery::Always`] as for
/// cleaner actions; under the default [`FinalizerDelivery::WhenLockHeld`]
/// only when there is a finalizer to run and `thread` holds a user-visible
/// lock (JLS §12.6) — then the delivery thread is started on first use and
/// this thread runs nothing. The delivery thread itself always keeps its own
/// batch.
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizer_thread_takes_finalizers(shared: &SharedVm, thread: &JvmThread) -> bool {
    match finalizer_delivery_policy() {
        FinalizerDelivery::Inline => false,
        FinalizerDelivery::Always => finalizer_thread_takes_delivery(shared, thread),
        FinalizerDelivery::WhenLockHeld => {
            if shared.mem.finalizer_thread.delivery_thread() == Some(thread.thread_id.0) {
                return true;
            }
            if shared.mem.finalizer_thread.pending_count() == 0 {
                return false;
            }
            // zt t26/x21: on ZGC never on the allocating thread
            // (`zgc_finalizers_off_caller`); zt t30/fz: nor on G1 /
            // Generational (`gc_finalizers_off_caller_all`).
            (finalizers_off_caller(shared) || thread_holds_user_locks(shared, thread))
                && request_finalizer_thread(shared, true)
        }
    }
}

/// `CRATONVM_ZGC_FINALIZERS_OFF_CALLER` (zt t26/x21, page
/// `zgc-zt13x10-review-residuals-CLOSED-20261019.md` items 22b / 25; default ON, `=0`
/// kill; ZGC only, both modes). HotSpot 25 runs every `finalize()` on its
/// `Finalizer` daemon, never on the thread that allocated or called
/// `System.gc()`. Under the default policy ([`FinalizerDelivery::WhenLockHeld`])
/// this VM handed finalizers to its delivery thread only when the draining
/// thread held a user-visible lock (JLS 12.6) and ran them inline otherwise:
/// rows `x16_F1` (`--real-jdk`), `x16_F1c`, `x16_F2c`, `x16_F3c`
/// (`GciFl22FinalizerThreadProbe`) printed `ran-on-caller true` /
/// `ran-on-daemon-only false` where HotSpot prints `false` / `true`. With the
/// switch the allocation doors ([`finalizer_thread_takes_finalizers`]) and the
/// collection doors ([`run_finalizers_for_collection_door`]) hand them to the
/// delivery thread whenever one serves or can be started; a VM that cannot
/// start it keeps the inline drain. Cleaner actions, reference-queue wake-ups,
/// GC notifications and `Runtime.runFinalization()` keep their own rules.
/// (The 2026-09-28 rejection of the whole hand-off cited the retired "keep
/// `--compatible` unchanged" rule; this is the `finalize()` part only, ZGC.)
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_finalizers_off_caller(shared: &SharedVm) -> bool {
    shared.mem.heap.is_zgc()
}

/// `CRATONVM_GC_FINALIZERS_OFF_CALLER_ALL` (zt t30/fz, page
/// `g1-and-generational-items-carried-from-the-zgc-zt-round-FIXED-20261023.md` item
/// 10b, the G1 / Generational half of `zgc-zt27fin1-finalizable-churn-and-signals-CLOSED-20261022.md`
/// items 1a / 3 / 3a; default ON, `=0` kill; every collector but ZGC, which
/// keeps its own keys; both modes). HotSpot 25 runs `finalize()` on its
/// `Finalizer` daemon and `runFinalization()` on a joined "Secondary
/// finalizer", never on a mutator; rows `x21_F1g` and `rf28_g1` (G1, and
/// Serial alike) printed `ran-on-caller true`. With the switch, under the
/// default policy: the allocation doors ([`finalizer_thread_takes_finalizers`])
/// hand the queue to the delivery thread whether or not the thread holds a
/// lock; a lock-free collection door ([`collection_door_hands_off_finalizers`])
/// hands it over and waits for it, bounded; `runFinalization`
/// ([`run_finalization_off_caller`]) does too, with the caller's drain as the
/// fallback; and the delivery thread runs its finalizer step in slices
/// ([`delivery_slices_finalizers`]), so a finalizable churn no longer holds
/// the GC notifications behind one unbounded step (fin1 item 1a). A
/// `finalize()` that sleeps runs at top level on the delivery thread, never
/// nested in an allocation door (fin1 item 3). No door waits for a finalizer
/// to free memory before it throws `OutOfMemoryError`: the allocation-failure
/// ladder only collects, as before; the queued objects stay reachable until
/// the delivery thread has run them, as HotSpot's do until `Finalizer` has.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_finalizers_off_caller_all(shared: &SharedVm) -> bool {
    !shared.mem.heap.is_zgc()
}

/// [`zgc_finalizers_off_caller`] or [`gc_finalizers_off_caller_all`]: the
/// allocation and collection doors leave `finalize()` to the delivery thread
/// whatever locks the thread holds. zt t30/fz.
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizers_off_caller(shared: &SharedVm) -> bool {
    zgc_finalizers_off_caller(shared) || gc_finalizers_off_caller_all(shared)
}


/// zt t30/fz: [`gc_finalizers_off_caller_all`] is every collector but ZGC,
/// default ON with a `=0` kill, and turns on the allocation doors' hand-off,
/// the collection door's, `runFinalization`'s and the delivery slices; ZGC's
/// keys stay its own. A fixture VM (no owning `Arc`) cannot start the delivery
/// thread, so nothing is handed off and every door keeps its inline drain.


/// How long one slice of a forced wait lasts before the waiter checks whether
/// the delivery thread is blocked on a monitor the waiter itself holds.
pub(in crate::runtime::interpreter::gc_and_alloc) const DELIVERY_WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(10);

/// Outcome of [`wait_for_delivery`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::gc_and_alloc) enum DeliveryWait {
    /// A batch covering every request made before the wait has finished.
    Served,
    /// The finalizer timeout passed first.
    TimedOut,
    /// The delivery thread is BLOCKED entering a monitor the waiter holds: it
    /// cannot finish until the waiter returns, so waiting longer is pointless.
    BlockedOnCaller,
}

/// Wait, in a blocking region (the delivery thread's Java may allocate and
/// collect meanwhile) and for at most the finalizer timeout, until a batch
/// covering every request made so far has finished.
///
/// When `caller_holds_locks`, the wait is sliced and between slices checks
/// whether the delivery thread is blocked entering a monitor this thread owns
/// ([`delivery_thread_waits_on_our_lock`]) — `synchronized (L) { … System.gc(); … }`
/// with a finalizer that synchronizes on `L`. Before w4-d that shape stalled
/// `System.gc()` for the full 2 s every time; it now returns as soon as the
/// block is seen. (A `java.util.concurrent` lock parks rather than blocks and
/// is not detected; the 2-s bound still holds for it.)
pub(in crate::runtime::interpreter::gc_and_alloc) fn wait_for_delivery(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller_holds_locks: bool,
) -> DeliveryWait {
    wait_for_delivery_step(shared, thread, caller_holds_locks, DeliveryStep::Batch)
}

/// What [`wait_for_delivery_step`] waits for. zt t15/x12.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::gc_and_alloc) enum DeliveryStep {
    /// A batch covering every request made before the wait has finished
    /// ([`wait_for_delivery`]).
    Batch,
    /// The batch in flight is not running finalizers it dequeued
    /// (`CRATONVM_ZGC_SYSTEM_GC_WAITS_FINALIZER_STEP_ONLY`).
    Finalizers,
}

/// [`wait_for_delivery`] for `step`: the same blocking region, timeout and
/// lock-holder slices; only the condition differs. zt t15/x12.
pub(in crate::runtime::interpreter::gc_and_alloc) fn wait_for_delivery_step(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller_holds_locks: bool,
    step: DeliveryStep,
) -> DeliveryWait {
    use cratonvm_native_api::NativeThreadAccess;
    let caller = thread.thread_id;
    let signal = shared.mem.finalizer_thread.delivery_signal();
    let target = signal.ticket();
    let done = |slice: std::time::Duration| match step {
        DeliveryStep::Batch => signal.wait_for_ticket(target, slice),
        DeliveryStep::Finalizers => signal.wait_for_finalizer_step(slice),
    };
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(shared.mem.finalizer_thread.timeout_ms());
    let mut ctx = crate::vm::NativeContextImpl {
        shared,
        thread: &mut *thread,
    };
    // Interpreter round i3 wave 81, lane L6: a `System.gc()` caller waiting
    // for the collection's reference delivery is in the VM on HotSpot (the
    // collection is a VM operation): RUNNABLE, not WAITING.
    ctx.begin_vm_wait_region();
    let outcome = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break if done(std::time::Duration::ZERO) {
                DeliveryWait::Served
            } else {
                DeliveryWait::TimedOut
            };
        }
        let slice = if caller_holds_locks {
            left.min(DELIVERY_WAIT_SLICE)
        } else {
            left
        };
        if done(slice) {
            break DeliveryWait::Served;
        }
        if caller_holds_locks && delivery_thread_waits_on_our_lock(shared, caller) {
            break DeliveryWait::BlockedOnCaller;
        }
    };
    ctx.end_blocking_region();
    outcome
}

/// Whether the delivery thread is BLOCKED entering a monitor `caller` owns,
/// read from the same JMX lists `ThreadMXBean` reports (no heap access, safe
/// in a blocking region). The two snapshots are taken separately, so a
/// relocation between them can only make this miss (keep waiting), and a
/// false `true` costs only the ordering a lock-holding `System.gc()` caller
/// was never promised by HotSpot anyway.
pub(in crate::runtime::interpreter::gc_and_alloc) fn delivery_thread_waits_on_our_lock(shared: &SharedVm, caller: crate::ThreadId) -> bool {
    let Some(delivery) = shared.mem.finalizer_thread.delivery_thread() else {
        return false;
    };
    let registry = &shared.threads.thread_registry;
    let Some((Some(contended), ..)) = registry.jmx_lock_snapshot(crate::ThreadId(delivery)) else {
        return false;
    };
    registry
        .jmx_lock_snapshot(caller)
        .is_some_and(|(_, _, owned, ..)| owned.iter().any(|o| o.as_ptr() == contended.as_ptr()))
}

/// `System.gc()`'s CLEANER drain with the full hand-off on: request a batch
/// and wait (see [`wait_for_delivery`]). `true` when the queues were served by
/// the delivery thread; `false` when the hand-off is off, the caller IS the
/// delivery thread, no thread could be started, or the wait ended unserved —
/// the caller then drains inline, as before: that drain is
/// `Bits.reserveMemory`'s last resort, which throws the moment `System.gc()`
/// returns without the native memory freed.
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizer_thread_drained_for(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    if !finalizer_thread_enabled() {
        return false;
    }
    if shared.mem.finalizer_thread.delivery_thread() == Some(thread.thread_id.0) {
        return false;
    }
    if !wake_finalizer_thread(shared) {
        return false;
    }
    let holds = thread_holds_user_locks(shared, thread);
    wait_for_delivery(shared, thread, holds) == DeliveryWait::Served
}

/// The forced `finalize()` drain's hand-off (`System.gc()`,
/// `Runtime.runFinalization()`): `true` when the caller must NOT run
/// finalizers itself — the delivery thread served them, or it is still
/// running them and the caller holds a user-visible lock (JLS §12.6: then the
/// queue stays with the delivery thread, which finishes once the lock is
/// released, rather than being run here re-entrantly — what HotSpot's
/// `runFinalization` would do is deadlock). `false`: drain inline, as before.
///
/// Per policy: `Inline` never hands off; `Always` always does and, unserved,
/// falls back inline only for a caller holding no lock (the pre-w4 rule for
/// the w3 fallback, minus the §12.6 case); `WhenLockHeld` hands off only for a
/// caller holding a lock.
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizers_handed_off_for_forced_drain(
    shared: &SharedVm,
    thread: &mut JvmThread,
    leave_busy: bool,
) -> bool {
    let policy = finalizer_delivery_policy();
    if policy == FinalizerDelivery::Inline {
        return false;
    }
    if shared.mem.finalizer_thread.delivery_thread() == Some(thread.thread_id.0) {
        return false;
    }
    let holds = thread_holds_user_locks(shared, thread);
    let requested = match policy {
        FinalizerDelivery::Always => wake_finalizer_thread(shared),
        FinalizerDelivery::WhenLockHeld => {
            holds
                && request_finalizer_thread(shared, shared.mem.finalizer_thread.pending_count() > 0)
        }
        FinalizerDelivery::Inline => false,
    };
    if !requested {
        return false;
    }
    forced_drain_left_to_delivery(wait_for_delivery(shared, thread, holds), holds, policy, leave_busy)
}

/// gci i35/jf: after [`finalizers_handed_off_for_forced_drain`]'s wait, may
/// the caller leave the queue to the delivery thread? Served: yes. Unserved:
/// a lock holder must (JLS §12.6); under the full hand-off a collection door
/// with `leave_busy` does too -- the thread is alive and owns the queue, and
/// HotSpot's `System.gc()` never runs a finalizer on its caller (page
/// `gci-i35jf-system-gc-runs-queued-finalizers-on-the-caller-FIXED-20261016`).
/// Otherwise the caller drains inline, as before. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn forced_drain_left_to_delivery(
    wait: DeliveryWait,
    holds: bool,
    policy: FinalizerDelivery,
    leave_busy: bool,
) -> bool {
    match wait {
        DeliveryWait::Served => true,
        DeliveryWait::TimedOut | DeliveryWait::BlockedOnCaller => {
            holds || (leave_busy && policy == FinalizerDelivery::Always)
        }
    }
}

/// Spawn this VM's delivery thread. `true` when it was spawned (or another
/// caller is spawning it), `false` when it could not be.
pub(in crate::runtime::interpreter::gc_and_alloc) fn start_finalizer_thread(shared: &SharedVm) -> bool {
    // Phase 5 fast boot (WP3): a boot image's capture run starts no OS thread
    // before its snapshot point; `false` keeps the inline drain, as for a VM
    // with no owning handle below. The first delivery after the restore
    // spawns the thread.
    if cratonvm_types::image_layout::capturing() {
        return false;
    }
    let ft = &shared.mem.finalizer_thread;
    if !ft.claim_delivery_thread_start() {
        ft.request_delivery();
        return true;
    }
    // A `SharedVm` not built by `Vm::new` (unit fixtures) has no owning handle
    // to give a thread; such a VM keeps the inline drain.
    let Some(owner) = shared.try_get_arc() else {
        ft.release_delivery_thread_start();
        return false;
    };
    let weak = std::sync::Arc::downgrade(&owner);
    drop(owner);
    let signal = ft.delivery_signal();
    // Queued before the thread exists; its first batch serves it.
    signal.request();
    let tid = shared.threads.thread_registry.next_thread_id();
    // STARTING, daemon: not counted by any STW census until the thread itself
    // passes `mark_stw_ready`, and never waited for at exit.
    shared
        .threads
        .thread_registry
        .register_starting_with_daemon(tid, FINALIZER_THREAD_NAME, None, true);
    // Same native stack as a `Thread.start()` worker (see `thread_start`).
    let stack_size = cratonvm_types::flags::runtime_var("RUST_MIN_STACK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(8 * 1024 * 1024);
    let spawned = std::thread::Builder::new()
        .name(FINALIZER_THREAD_NAME.to_string())
        .stack_size(stack_size)
        .spawn(move || finalizer_thread_main(weak, signal, tid, stack_size));
    match spawned {
        // gen r5w5/conc9: the handle goes to the registry, as a
        // `Thread.start()` worker's does, so a delivery thread that UNWINDS
        // past `mark_dead` (a Rust panic under a finalizer) is dead to every
        // stop-the-world census (`entry_is_stw_live`) instead of being waited
        // for, running, forever. A normal exit marks itself dead first and the
        // handle is detached there. Found by the service-thread audit
        // (`gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection`).
        Ok(handle) => {
            shared.threads.thread_registry.set_join_handle(tid, handle);
            true
        }
        Err(e) => {
            shared.threads.thread_registry.mark_dead(tid);
            ft.release_delivery_thread_start();
            tracing::warn!(
                target: "cratonvm::gc::finalizer",
                error = %e,
                "could not start the reference-delivery thread; finalizers and cleaner \
                 actions keep running inline on the collecting thread"
            );
            false
        }
    }
}

/// Body of the delivery thread. Mirrors the worker wiring of `thread_start`
/// (`vm_exec.rs`) for everything a GC initiator reads about a thread, and the
/// idle discipline of the JNI AIO dispatcher (`jni.rs`, `aio_dispatcher_main`):
/// GC-blocked while idle, a counted mutator only while running a batch.
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizer_thread_main(
    weak: std::sync::Weak<SharedVm>,
    signal: std::sync::Arc<cratonvm_gc::reference::DeliverySignal>,
    tid: crate::ThreadId,
    stack_size: usize,
) {
    use cratonvm_native_api::NativeThreadAccess;
    crate::runtime::interpreter::init_thread_exec_depth_ceiling(stack_size);
    let Some(mut shared) = weak.upgrade() else {
        return;
    };
    // gci i42/rp: an unwind out of this function retires the thread as the
    // VM's delivery thread (see `DeliveryThreadUnwind`).
    let _unwind = DeliveryThreadUnwind {
        weak: weak.clone(),
        tid,
    };
    // Never moved after its addresses are published below; dropped only when
    // the VM is gone.
    let mut thread = JvmThread::new(tid, FINALIZER_THREAD_NAME);
    // gci i45/rv: on an unwind, dead to every STW census BEFORE `thread`'s
    // published addresses go away (declared after it, so dropped first).
    let _registry_unwind = DeliveryThreadRegistryUnwind {
        weak: weak.clone(),
        tid,
    };
    // nio21 wave 3 L1: this VM's redefinitions only.
    thread
        .invoke_cache
        .bind_redefinition_log(shared.classes.redefinition_log.clone());
    thread.kind = crate::threading::ThreadKind::Platform;
    thread.daemon = true;
    // zwr: a take-over that freezes this thread reads its published Rust band
    // floor (`with_chain_covered_java`).
    thread
        .gc_block_state
        .delivery_thread
        .store(true, std::sync::atomic::Ordering::Release);
    {
        let registry = &shared.threads.thread_registry;
        registry.set_interrupted_flag(tid, thread.interrupted.clone());
        registry.set_park_state(tid, thread.park_state.clone());
        registry.set_root_snapshot(tid, thread.root_snapshot.clone());
        registry.set_frame_trace(tid, thread.frame_trace.clone());
        registry.set_vm_state(tid, thread.vm_state.clone());
        registry.set_gc_block_state(tid, thread.gc_block_state.clone());
        registry.set_tlab_addr(tid, &thread.tlab as *const cratonvm_gc::Tlab as usize);
        registry.set_jvm_thread_addr(tid, &thread as *const JvmThread as usize);
        registry.set_os_tid_current(tid);
    }
    cratonvm_classloading::set_current_thread_id(shared.classes.class_layout_domain, tid.0);
    let _crash_frames = crate::vm::PublishedFrameTrace::publish(
        FINALIZER_THREAD_NAME,
        tid.0,
        thread.frame_trace.clone(),
    );
    thread.set_vm_state("finalizer-thread:registered");
    // Become STW-visible exactly as a `Thread.start()` worker does.
    loop {
        let ready = shared.mem.gc_barrier.run_if_no_stw_requested(|| {
            shared.threads.thread_registry.mark_stw_ready(tid);
        });
        if ready {
            break;
        }
        let pointer_map = shared.mem.gc_barrier.arrive_and_wait_excluded(tid);
        if !pointer_map.is_empty() {
            apply_pointer_map_to_thread(
                &mut thread,
                &pointer_map,
                &shared.mem.heap,
                &shared.classes.type_maps,
            );
        }
    }
    if shared
        .mem
        .gc_barrier
        .stw_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        safepoint_check(&shared, &mut thread);
    }
    shared.mem.finalizer_thread.set_delivery_thread(tid.0);
    loop {
        // Idle: GC-blocked (deposited snapshot, retired TLAB), holding only
        // the weak handle, so a pause never waits for this thread and the VM
        // can be dropped under it.
        thread.set_vm_state("finalizer-thread:idle");
        {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            // Interpreter round i3 wave 81, lane L6: the VM's own thread
            // idling in the VM (no `java.lang.Thread`): RUNNABLE, as
            // HotSpot's service threads.
            // zt t32: no Java frame is on this stack between batches, so the
            // deposit roots none of the last batch's compiled-frame residue,
            // and the dead stack the batch used is zeroed: a later batch's
            // frames find no earlier batch's words in their never-written
            // slots, which a pause's conservative band scan would root.
            {
                scrub_dead_stack_below((stack_size / 8).min(DELIVERY_SCRUB_BYTES));
            }
            crate::jit::conservative_roots::with_no_java_on_stack(|| {
                ctx.begin_vm_wait_region();
            });
        }
        drop(shared);
        loop {
            if signal.wait_for_request(FINALIZER_THREAD_IDLE_TICK) {
                break;
            }
            if weak.strong_count() == 0 {
                return;
            }
        }
        shared = match weak.upgrade() {
            Some(s) => s,
            None => return,
        };
        {
            // Waits out an in-flight pause and applies what it moved.
            let mut ctx = crate::vm::NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            // zt t32: still no Java frame (the wake's refresh deposit).
            crate::jit::conservative_roots::with_no_java_on_stack(|| {
                ctx.end_blocking_region();
            });
        }
        thread.set_vm_state("finalizer-thread:batch");
        let ticket = signal.begin_batch();
        // zwr: every compiled frame of the batch is entered through a door
        // that registers it, so the probe for an unregistered one judges
        // residue (`with_chain_covered_java`).
        let block = std::sync::Arc::clone(&thread.gc_block_state);
        crate::jit::conservative_roots::with_chain_covered_java(&block.delivery_rust_floor, || {
            drain_finalizer_thread_batch(&shared, &mut thread);
        });
        signal.finish_batch(ticket);
    }
}

/// zt t32: how much dead stack the delivery thread zeroes below its idle frame.
pub(in crate::runtime::interpreter::gc_and_alloc) const DELIVERY_SCRUB_BYTES: usize = 1024 * 1024;

/// zwr: how much dead stack the delivery thread zeroes before each item of a
/// batch ([`scrub_delivery_dead_stack`]).
pub(in crate::runtime::interpreter::gc_and_alloc) const DELIVERY_ITEM_SCRUB_BYTES: usize = 256 * 1024;

/// zwr (`zgc-delivery-thread-conservative-jit-band-keeps-a-weak-referent-FIXED-20261024`):
/// on the delivery thread inside a batch, zero the dead stack the previous
/// item (a GC notification's listeners) ran on before the next one runs. The
/// t32 scrub does this once per batch, before the thread idles. Within a batch,
/// the next listener's compiled frames covered the same memory, and their
/// never-written slots kept the previous listener's words. The thread's own
/// safepoint deposit then read them conservatively: 14 copies of a dead
/// object's base, whose address a new `int[16]` had taken, kept that weak
/// referent (`Zt1MkSystemGcFresh`, one run in 3 000). A no-op on every
/// other thread, and with `CRATONVM_JIT_DELIVERY_BATCH_RESIDUE_JUDGED=0`.
pub(in crate::runtime::interpreter) fn scrub_delivery_dead_stack() {
    if crate::jit::conservative_roots::in_delivery_batch() {
        scrub_dead_stack_below(DELIVERY_ITEM_SCRUB_BYTES);
    }
}

/// zt t32 (the successor page's section 4, `alive_after_gc=1`): zero `bytes` of
/// the dead stack below the caller's frame. Frames of 64 KiB, each zeroed and
/// kept alive past its recursive call (so the recursion is not a loop that
/// reuses one frame). Only dead memory is written: everything below the
/// caller's stack pointer belongs to frames that have returned.
#[inline(never)]
pub(in crate::runtime::interpreter::gc_and_alloc) fn scrub_dead_stack_below(bytes: usize) {
    const CHUNK: usize = 64 * 1024;
    #[inline(never)]
    fn chunk(left: usize) {
        let mut buf = [0u8; CHUNK];
        std::hint::black_box(&mut buf);
        if left > CHUNK {
            chunk(left - CHUNK);
        }
        std::hint::black_box(&buf);
    }
    chunk(bytes);
}

/// gci i42/rp (`docs/feature-designs/real-jdk-mode.md`):
/// installed by [`finalizer_thread_main`] for its whole life. On an UNWIND out
/// of it (a Rust panic under a finalizer, a cleaner action or a GC listener;
/// the release profile unwinds) it retires the thread as this VM's delivery
/// thread (`FinalizerThread::retire_delivery_thread`): the start claim is
/// handed back and the dead id unpublished, so the next request starts a
/// fresh thread whose first batch serves everything still queued. Before
/// this the claim stayed taken and `delivery_thread()` kept naming the dead
/// thread, so `request_finalizer_thread` answered "served" forever: every
/// `System.gc()` waited out the finalizer timeout, and under the full hand-off
/// (and a `--real-jdk` VM's `ReferenceQueue` wake-ups) nothing was delivered
/// again. A normal return (the VM is gone) changes nothing. Atomics only:
/// safe while unwinding.
pub(in crate::runtime::interpreter::gc_and_alloc) struct DeliveryThreadUnwind {
    pub(in crate::runtime::interpreter::gc_and_alloc) weak: std::sync::Weak<SharedVm>,
    pub(in crate::runtime::interpreter::gc_and_alloc) tid: crate::ThreadId,
}

impl Drop for DeliveryThreadUnwind {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        if let Some(shared) = self.weak.upgrade() {
            let _ = shared
                .mem
                .finalizer_thread
                .retire_delivery_thread(self.tid.0);
        }
    }
}

/// gci i45/rv (`gci-i45rv-waves-i41-i44-review-findings-FIXED-20261016` item 1): an
/// unwinding delivery thread marks its registry entry dead before its
/// `JvmThread` (whose addresses the registry publishes) is dropped, so no STW
/// census counts a thread that will never arrive. A normal return changes
/// nothing (the VM is gone).
pub(in crate::runtime::interpreter::gc_and_alloc) struct DeliveryThreadRegistryUnwind {
    pub(in crate::runtime::interpreter::gc_and_alloc) weak: std::sync::Weak<SharedVm>,
    pub(in crate::runtime::interpreter::gc_and_alloc) tid: crate::ThreadId,
}

impl Drop for DeliveryThreadRegistryUnwind {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        if let Some(shared) = self.weak.upgrade() {
            shared.threads.thread_registry.mark_dead(self.tid);
        }
    }
}

/// One batch of the delivery thread: queue wake-ups, `finalize()`, cleaner
/// actions, then any wake-ups the batch's own collections queued. Runs on the
/// delivery thread at top level (no JIT borrow), so the non-forced drains run.
///
/// Also delivers any queued GC notifications (JMX `NotificationListener`s,
/// `run_gc_notifications`) whenever the doors hand them here — the full
/// hand-off, and since gc-common w6-d every `--real-jdk` VM
/// (`gc_notification_delivery`). Same one-drainer-at-a-time queue
/// the doors drain. A `--compatible` VM's doors deliver their own unless the
/// collecting thread holds a user-visible lock (gc-common w7-d, see
/// [`gc_notifications_go_to_delivery_thread`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn drain_finalizer_thread_batch(shared: &SharedVm, thread: &mut JvmThread) {
    // gc-common w4-d: under the default policy the thread exists only to run
    // the finalizers a lock-holding mutator could not (JLS §12.6); cleaner
    // actions, queue wake-ups (never recorded then) and GC notifications keep
    // their inline doors, so `--compatible` output changes only in that case.
    //
    // gc-common w5-d: except that a `--real-jdk` VM records its queue wake-ups
    // under the default policy too (`queue_wake_ups_recorded_for`), and this is
    // where they are delivered. Under `--compatible` nothing is ever recorded,
    // so both calls take the empty list and return at once.
    if !finalizer_thread_enabled() {
        // zt t27/fin1 (`CRATONVM_ZGC_DELIVERY_SLICES_FINALIZERS`): on ZGC the
        // finalizer step runs in slices, with the queue wake-ups and the GC
        // notifications delivered between them; the batch still ends only
        // once a slice finds the queue empty (or runs nothing).
        let sliced = delivery_slices_finalizers(shared);
        gc_enqueued_queues_notify(shared, thread);
        loop {
            let ran = run_delivery_finalizer_step(shared, thread, sliced);
            gc_enqueued_queues_notify(shared, thread);
            // gc-common w6-d: and a `--real-jdk` VM's GC notifications, which
            // its doors hand here (`gc_notification_delivery`).
            // gc-common w7-d: a `--compatible` VM's doors hand them here only
            // when the collecting thread held a user-visible lock, and mark
            // it; the mark is taken AFTER the finalizers, so a finalizer that
            // allocated under a lock is covered too. Unmarked, a lock-free
            // door is delivering its own and this batch leaves them to it.
            match gc_notification_delivery(shared) {
                GcNotificationDelivery::OffMutator => {
                    run_gc_notifications_as_notification_thread(shared, thread)
                }
                GcNotificationDelivery::WhenLockHeld => {
                    if shared
                        .mem
                        .finalizer_thread
                        .take_gc_notifications_handed_off()
                    {
                        run_gc_notifications_as_notification_thread(shared, thread);
                    }
                }
                GcNotificationDelivery::Inline => {}
            }
            if !sliced || ran == 0 || shared.mem.finalizer_thread.pending_count() == 0 {
                return;
            }
        }
    }
    gc_enqueued_queues_notify(shared, thread);
    run_finalizers_impl(shared, thread, false);
    run_cleaner_actions_impl(shared, thread, false);
    gc_enqueued_queues_notify(shared, thread);
    run_gc_notifications_as_notification_thread(shared, thread);
}

/// The longest one finalizer slice of [`run_delivery_finalizer_slice`] runs
/// before the batch delivers the GC notifications and queue wake-ups queued
/// meanwhile (zt t27/fin1).
pub(in crate::runtime::interpreter::gc_and_alloc) const DELIVERY_FINALIZER_SLICE: std::time::Duration = std::time::Duration::from_millis(20);

/// The default policy's finalizer step of one delivery batch: the step a
/// lock-free `System.gc()` waits for (`system_gc_waits_finalizer_step_only`,
/// zt t15/x12), ended on unwind too, so a panicking `finalize()` cannot hold
/// every later caller for the timeout. `sliced`: at most one
/// [`run_delivery_finalizer_slice`]; otherwise the whole queue, a peer's
/// additions included. Answers how many objects were dequeued.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_delivery_finalizer_step(shared: &SharedVm, thread: &mut JvmThread, sliced: bool) -> usize {
    struct FinalizerStep(std::sync::Arc<cratonvm_gc::reference::DeliverySignal>);
    impl Drop for FinalizerStep {
        fn drop(&mut self) {
            self.0.end_finalizer_step();
        }
    }
    let census = finalizer_delivery_census_on();
    let t0 = census.then(std::time::Instant::now);
    let signal = shared.mem.finalizer_thread.delivery_signal();
    signal.begin_finalizer_step();
    let _step = FinalizerStep(signal);
    let ran = if sliced {
        run_delivery_finalizer_slice(shared, thread)
    } else {
        run_finalizers_impl(shared, thread, false)
    };
    if let Some(t0) = t0.filter(|_| ran > 0) {
        eprintln!(
            "[fin-delivery] ran={ran} left={} step_us={} sliced={sliced}",
            shared.mem.finalizer_thread.pending_count(),
            t0.elapsed().as_micros()
        );
    }
    ran
}

/// zt t27/fin1: one slice of the delivery thread's finalizer step -- at most
/// the objects queued when it began, and no longer than
/// [`DELIVERY_FINALIZER_SLICE`] (checked after each `finalize()`). Answers how
/// many objects it dequeued.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_delivery_finalizer_slice(shared: &SharedVm, thread: &mut JvmThread) -> usize {
    let limit = shared.mem.finalizer_thread.pending_count();
    let deadline = std::time::Instant::now() + DELIVERY_FINALIZER_SLICE;
    let mut ran = 0usize;
    while ran < limit {
        let one = run_finalizers_impl_upto(shared, thread, false, 1);
        if one == 0 {
            break;
        }
        ran += one;
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
    ran
}

/// `CRATONVM_ZGC_DELIVERY_SLICES_FINALIZERS` (zt t27/fin1, page
/// `zgc-zt27fin1-finalizable-churn-and-signals-CLOSED-20261022.md` item 1; default
/// ON, `=0` kill; ZGC only, both modes, default finalizer policy). HotSpot 25
/// runs `finalize()` on its `Finalizer` daemon and delivers GC notifications
/// on its `Notification Thread`: a finalizer backlog never holds a
/// notification back. This VM's delivery thread does both, in that order, in
/// one batch whose finalizer step ran until the queue was empty -- and with
/// every `finalize()` on that thread (`CRATONVM_ZGC_FINALIZERS_OFF_CALLER`) a
/// peer that keeps making finalizable garbage (`Zt13X10MissedDrain 40 fin`)
/// keeps the step running for as long as it churns, so every GC notification
/// and `ReferenceQueue` wake-up of those collections waited for the churn to
/// stop and then for the backlog. With the switch the step runs in slices of
/// at most the queue length at the slice's start and
/// [`DELIVERY_FINALIZER_SLICE`], and the batch delivers what is queued between
/// slices; it still ends only with the queue empty, so `runFinalization` and
/// the forced waits keep their promise (the objects a slice dequeued have run
/// when its step ends). `=0`: one unbounded step, as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_delivery_slices_finalizers(shared: &SharedVm) -> bool {
    shared.mem.heap.is_zgc()
}

/// The delivery thread's finalizer step runs in slices: ZGC under
/// [`zgc_delivery_slices_finalizers`], G1 / Generational under
/// [`gc_finalizers_off_caller_all`] (zt t30/fz: with every `finalize()` on
/// the one delivery thread, fin1 item 1a's reason).
pub(in crate::runtime::interpreter::gc_and_alloc) fn delivery_slices_finalizers(shared: &SharedVm) -> bool {
    zgc_delivery_slices_finalizers(shared) || gc_finalizers_off_caller_all(shared)
}

/// `CRATONVM_DBG_FINALIZER_DELIVERY=1` (zt t27/fin1, diagnostic, default
/// OFF): one stderr line per delivery-thread finalizer step that ran
/// anything, `[fin-delivery] ran=N left=M step_us=T sliced=<bool>`, so a run
/// whose `System.gc()` rounds slow down or never end can be matched to the
/// delivery thread's throughput and backlog (rate = ran / step_us).
pub(in crate::runtime::interpreter::gc_and_alloc) fn finalizer_delivery_census_on() -> bool {
    false
}


/// HotSpot's name for the thread its GC notifications run on
/// (`NotificationThread`, JDK 14+; the `Service Thread` before).
pub(in crate::runtime::interpreter::gc_and_alloc) const NOTIFICATION_THREAD_NAME: &str = "Notification Thread";

/// The delivery thread's GC-notification step
/// ([`run_gc_notifications`]), run under HotSpot's thread name. gcd d10/o
/// (2026-09-28), item 2 of
/// `docs/feature-designs/real-jdk-mode.md`.
///
/// A `NotificationListener` on a GC bean that asked `Thread.currentThread()
/// .getName()` answered `Craton Finalizer` -- this VM's reference-delivery
/// thread, which also runs `finalize()` and cleaner actions -- where HotSpot
/// answers `Notification Thread` on every collector
/// (`GcNotificationThreadProbe`: `threads=[Notification Thread]` on Serial,
/// G1 and ZGC, Temurin 25.0.3). One thread still does all three jobs; only
/// the NAME the listeners see follows the job: for the duration of the
/// delivery, the Rust-side name (what a mirror built lazily during the
/// delivery is named from, `build_current_thread_object`) and the Java
/// mirror's `name` field say `Notification Thread`, and both are put back
/// afterwards, so a `finalize()` on this thread sees what it saw before.
///
/// Heap-safe by construction: the two renames allocate one short String each
/// through the single-attempt, non-collecting
/// `try_create_java_string_from_units` (no `JvmThread` is handed to it, so it
/// can neither collect nor park at a safepoint, and the mirror read before it
/// is still current after it). A refused allocation leaves the name as it was
/// -- a wrong name, never a failed delivery. A listener that calls
/// `setName` on this thread has its name replaced when the delivery ends.
///
/// Only the delivery thread's own deliveries are renamed. A `--compatible`
/// door that delivers inline (a lock-free collecting thread, gc-common w7-d)
/// still runs the listeners on the collecting thread under its own name;
/// that is the policy [`gc_notifications_go_to_delivery_thread`] states.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_gc_notifications_as_notification_thread(shared: &SharedVm, thread: &mut JvmThread) {
    if !super::gc_events::gc_notifications_pending(shared) {
        return;
    }
    let previous = std::mem::replace(&mut thread.name, NOTIFICATION_THREAD_NAME.to_string());
    set_delivery_mirror_name(shared, thread);
    run_gc_notifications(shared, thread, false);
    thread.name = previous;
    set_delivery_mirror_name(shared, thread);
}

/// Write `thread.name` into its Java mirror's `name` field, when the thread
/// has a mirror and its class declares the field (a unit VM's synthetic
/// `Thread` may not). See [`run_gc_notifications_as_notification_thread`] for
/// why the allocation cannot move the mirror.
pub(in crate::runtime::interpreter::gc_and_alloc) fn set_delivery_mirror_name(shared: &SharedVm, thread: &JvmThread) {
    let Some(mirror) = thread.java_thread_obj else {
        return;
    };
    let class_id = shared.mem.heap.class_id_of(mirror);
    let Some(slot) =
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, class_id, "name")
    else {
        return;
    };
    let units: Vec<u16> = thread.name.encode_utf16().collect();
    let Some(name) = crate::vm::try_create_java_string_from_units(shared, &units) else {
        return;
    };
    shared
        .mem
        .heap
        .set_field(mirror, slot, Value::Object(Some(name)));
}

/// Do for every queue the collector spliced a `Reference` onto what the JDK's
/// `ReferenceQueue.enqueue0` does after linking: `synchronized (lock) {
/// lock.notifyAll(); }` — so a thread already blocked in `remove()` sees it.
///
/// `common-w2f-gc-enqueue-never-wakes-queue-waiters-FIXED-20260923.md`:
/// under `--real-jdk` the real `remove()` is `lock.wait()` and nothing woke it
/// — the Common Cleaner learned of a GC-enqueued `PhantomCleanable` only at
/// its 60 s timeout, and an untimed `remove()` possibly never. Taking the
/// monitor (not a bare wake) is what makes this race-free: a waiter between
/// its empty `poll0()` and its `wait()` holds `lock`, so the notify lands
/// after it waits.
///
/// Only real-layout queues have `lock`; the synthetic two-slot shape and a
/// queue whose `lock` is still null are skipped (nothing can be waiting on a
/// monitor there — the default mode's natives poll). Delivery thread only:
/// the monitor enter may block, which is why the pause cannot do this.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_enqueued_queues_notify(shared: &SharedVm, thread: &mut JvmThread) {
    use cratonvm_native_api::{NativeHeapAccess as _, NativeThreadAccess as _};
    let queues = shared.mem.cleaner_thread.drain_queues_to_notify();
    if queues.is_empty() {
        return;
    }
    // Pinned for the batch: each monitor enter may wait out a moving pause.
    let pin_base = thread.native_pin_roots.len();
    for &addr in &queues {
        if shared.mem.heap.is_heap_addr(addr).is_none() {
            continue;
        }
        // SAFETY: recorded by a reference pass from a live, shape-screened
        // `ReferenceQueue`, rooted (`CleanerThread::pending_addresses`) and
        // relocated (`CleanerThread::update_after_gc`) on every collection
        // since, and nothing has run between the drain above and here.
        thread
            .native_pin_roots
            .push(unsafe { ObjectRef::from_raw(addr as *mut u8) });
    }
    let pin_end = thread.native_pin_roots.len();
    for pin in pin_base..pin_end {
        let queue = thread.native_pin_roots[pin];
        // Not an array: its header would carry the COMPONENT's class id, and a
        // `ReferenceQueue[]` would pass as a queue (see `is_cleanable_shaped`).
        let is_array = shared.mem.heap.kind_of(queue) == crate::memory::heap::ObjectKind::Array;
        let shaped = !is_array && {
            let cm = shared.classes.class_manager.read();
            let cid = shared.mem.heap.class_id_of(queue);
            cm.is_assignable_to_name(cid, "java/lang/ref/ReferenceQueue")
        };
        if !shaped {
            continue;
        }
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        // gc-common w6-d: `ReferenceQueue`'s OWN `lock`, resolved on the
        // declaring class. By name on the receiver's class, a subclass that
        // declares a field of its own called `lock` (`class Q extends
        // ReferenceQueue<Object> { final Object lock = new Object(); }`) got
        // this notify, and a waiter on the real `lock` — the JDK's `remove()`
        // bytecode, or the native `remove` since w6-d (`rq_wait_on_lock`) —
        // slept on.
        let Some(lock_slot) = ctx.resolve_field_index("java/lang/ref/ReferenceQueue", "lock")
        else {
            continue;
        };
        if lock_slot >= ctx.object_num_fields(queue) {
            continue;
        }
        let Value::Object(Some(lock)) = ctx.get_field(queue, lock_slot) else {
            continue;
        };
        // The gc-safe enter: a contended wait is excused from a pause, so the
        // monitor object may move; use what it returns.
        let lock = ctx.monitor_enter_gc_safe(lock);
        let _ = ctx.monitor_notify_all(lock);
        ctx.monitor_exit(lock);    }
    thread.native_pin_roots.truncate(pin_base);
}

/// Resolve the field slot used to link a `java.lang.ref.Reference` (or
/// subclass instance) onto its `ReferenceQueue`'s linked list when the GC
/// auto-enqueues it -- the `next` field as declared on `java/lang/ref/Reference`
/// itself (referent, queue, next, discovered — real-JDK layout).
///
/// MUST be resolved BY NAME against `Reference`'s own declaring class, not
/// by a field-count heuristic on the receiver's most-derived class: a
/// `java.util.WeakHashMap$Entry` (itself a `WeakReference` subclass) also
/// declares its OWN field named `next` (used for its hash-BUCKET chain, a
/// completely different linked list). The old heuristic ("index 2 if the
/// object has more than 2 fields") cannot tell these apart — for a
/// `WeakHashMap$Entry` specifically it can land on the wrong `next` slot,
/// so a GC-driven auto-enqueue (a live-application-scale WeakHashMap
/// *will* eventually have a stale entry to expunge) splices the
/// ReferenceQueue's link over the bucket-chain link, corrupting whatever
/// hash bucket that entry lived in — a later `WeakHashMap.get()`/
/// `expungeStaleEntries()` walking that bucket's `Entry.next` chain then
/// loops forever (observed: `com.sun.beans.TypeResolver`'s internal
/// `WeakCache`'s `WeakHashMap.get()` permanently stuck inside
/// `matchesKey()`, hanging Spring Boot's Thymeleaf layout-dialect
/// `createLayoutFromConfigClass` test). See
/// thymeleaf-groovy-layoutdialect-metaclass-introspection-hang-FIXED.md.
pub(in crate::runtime::interpreter) fn gc_reference_next_slot(shared: &SharedVm) -> usize {
    let cm = shared.classes.class_manager.read();
    cm.find_bootstrap_class_by_name("java/lang/ref/Reference")
        .and_then(|reference_cid| {
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(
                reference_cid,
                "next",
                cm.class_store(),
            )
        })
        // `java/lang/ref/Reference` should always be loaded by the time any
        // Reference object exists to enqueue; this fallback only guards
        // against that invariant somehow not holding.
        .unwrap_or(2)
}

/// True when the object at `cleanable` still has a shape a pending Cleaner
/// action can legitimately have.
///
/// Two shapes reach the cleaner queue, and both are checked because both are
/// registered: the real JDK's `jdk.internal.ref.Cleaner` and
/// `jdk.internal.ref.PhantomCleanable` are `java.lang.ref.Reference` subclasses
/// (discovered from the `PhantomReference.<init>` native), and the synthetic
/// Cleanable that `phases_late`/`servlet` build implements
/// `java.lang.ref.Cleaner$Cleanable`. Anything else at the address means the
/// address was reclaimed and its slot reused.
///
/// Why it matters, and it is not the same bug as the queue-head one:
/// `run_cleaner_actions_impl` WRITES slot 1 (the cleaned flag) and NULLS slot 0
/// (the action) before it invokes anything. Through a reused address that is a
/// field-0 null on an innocent object — on a `java.lang.String` it nulls
/// `value`, and the `byte[]` that vanishes surfaces far away, in a frame with no
/// connection to the GC, as H2 `TestMultiThread.testViews`'
/// `NullPointerException: Cannot read the array length because "<local4>" is
/// null`. Measured 2026-08-16: that NPE outlived the `ReferenceQueue` fixes in
/// this file and was still reproducible under `-XX:+UseGenerationalGC`, which is
/// how this second producer was separated from the first.
pub(in crate::runtime::interpreter::gc_and_alloc) fn is_cleanable_shaped(
    cm: &crate::classloading::ClassManager,
    shared: &SharedVm,
    cleanable: ObjectRef,
) -> bool {
    // An array's header carries its COMPONENT's class id, so a
    // `ThreadLocalMap.Entry[]` (Entry extends WeakReference) would pass as
    // Reference-shaped and have its elements read as fields. Same screen as
    // `is_reference_shaped` in the enqueue loop.
    if shared.mem.heap.kind_of(cleanable) == crate::memory::heap::ObjectKind::Array {
        return false;
    }
    let cid = shared.mem.heap.class_id_of(cleanable);
    cm.is_assignable_to_name(cid, "java/lang/ref/Reference")
        || cm.is_assignable_to_name(cid, "java/lang/ref/Cleaner$Cleanable")
}

/// `java.lang.ref.ReferenceQueue.ENQUEUED`, at its POST-collection address, for
/// the GC's own auto-enqueue to store in a real-layout `Reference.queue`.
/// `None` when the class or its static is not there yet (nothing to store but
/// the synthetic mark).
///
/// **W2-F-3 (2026-09-23).** The two GC enqueue loops (`process_references_after_gc`
/// and `g1_remark_process_references`) marked an enqueued reference by writing
/// the synthetic `Int(1)` into slot 1. On the two-slot synthetic shape that
/// slot is untyped and the sentinel survives. On the real JDK layout slot 1 is
/// `queue : Ljava/lang/ref/ReferenceQueue;`, where a primitive is coerced to
/// null (the G30-1 slot coercion `native_ref_is_enqueued`'s doc describes), so
/// every reference the COLLECTOR enqueued read back with `queue == null`:
///
/// * `isEnqueued()` answered `false` for it until polled (HotSpot: `true`) —
///   the "one remaining shape" G49-1 §NOMINATION left open;
/// * real `Reference.enqueue()` bytecode (`this.queue.enqueue(this)`, the
///   path `--real-jdk` runs) threw `NullPointerException` where HotSpot
///   answers `false`;
/// * `ReferenceQueue.enqueue0`'s own "already enqueued" test
///   (`queue == ENQUEUED`) could not see it.
///
/// The JDK's `enqueue0` stores `ENQUEUED`; so does this now.
///
/// `pointer_map` relocates the static's value: the VM's statics are remapped
/// by `update_all_roots`, which every caller runs AFTER the reference pass, so
/// they still hold the pre-collection address here. The caller must still
/// shape-check the result (a `ReferenceQueue`) and fall back to `Int(1)`.
///
/// Deliberately uncached: a cache would be a process global holding one VM's
/// class id, and this runs once per collection.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_enqueued_sentinel(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
) -> Option<ObjectRef> {
    let (class_id, index) = {
        let cm = shared.classes.class_manager.read();
        let class_id = cm.find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue")?;
        let class = cm.get_class(class_id)?;
        let index = class
            .fields
            .iter()
            .filter(|f| f.is_static())
            .position(|f| &*f.name == "ENQUEUED")?;
        (class_id, index)
    };
    match crate::vm::get_static_shared(shared, class_id, index) {
        Value::Object(Some(sentinel)) => {
            let addr = sentinel.as_ptr() as usize;
            let now = pointer_map.get(&addr).copied().unwrap_or(addr);
            // SAFETY: the static's value, relocated through this collection's
            // own map; the caller shape-checks it before writing it anywhere.
            Some(unsafe { ObjectRef::from_raw(now as *mut u8) })
        }
        _ => None,
    }
}

/// The value the GC enqueue loops store in `Reference.queue`: the JDK's
/// `ReferenceQueue.ENQUEUED` on the real layout (`next_slot != 0`, i.e. the
/// Reference has a `next` field), the synthetic `Int(1)` otherwise or when the
/// sentinel is unavailable. See [`gc_enqueued_sentinel`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_enqueued_mark(sentinel: Option<ObjectRef>, next_slot: usize) -> Value {
    match sentinel {
        Some(s) if next_slot != 0 => Value::Object(Some(s)),
        _ => Value::Int(1),
    }
}

/// gci i16/fr (`gci-i15rf-g1-remark-loses-a-reference-only-a-dead-finalizable-reaches-RETIRED-UNFIXED-20261018`,
/// the stop-the-world half): `CRATONVM_STW_UNDISCOVERED_REFS_STRONG` --
/// default OFF (`runtime_flag_on`; it changes what a pause keeps). Read once
/// per stop-the-world pause, and only when a finalizable object is
/// registered; no `static`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn stw_undiscovered_refs_strong_enabled() -> bool {
    true
}

/// gci i48/rf48 (`gci-i15rf-...-20261014` residual): does this pause run
/// the [`StwUndiscoveredProbe`]?
/// Under the opt-in `CRATONVM_STW_UNDISCOVERED_REFS_STRONG` every pause does,
/// as before. Otherwise only a G1 pause on a full-collection door
/// ([`g1_full_collection_door`]: its young pause and its compaction pause),
/// under the opt-in `CRATONVM_G1_FULL_GC_KEEPS_UNDISCOVERED_REFS` (default
/// OFF; it was the G-9 switch's default path until the i48p rows): those pauses and the cycle between them are
/// HotSpot's ONE full collection, whose keep-alive phase traces the referent
/// of a `Reference` only a dead finalizable reaches, and the cycle's remark
/// already keeps it (`g1_remark_undiscovered_references`). Without the probe
/// the young pause nulled that slot, kept the `Reference` through the
/// finalizer resurrection and freed the referent before the remark could
/// see it (`GciI16FrStwUndiscoveredProbe nested|twin` on G1, every arm).
pub(in crate::runtime::interpreter::gc_and_alloc) fn stw_probe_engaged(shared: &SharedVm, door: GcDoor) -> bool {
    stw_undiscovered_refs_strong_on(shared) || g1_full_gc_keeps_undiscovered(shared, door)
}

/// WARNING flip (gcj j2, 2026-10-19, `triage-j1.md` vi16-fr): on the
/// Generational backend `CRATONVM_STW_UNDISCOVERED_REFS_STRONG` is default ON
/// (`=0` kills) -- HotSpot Serial keeps the referent of a `Reference` only a
/// dead finalizable reaches; G1 keeps the opt-in
/// ([`stw_undiscovered_refs_strong_enabled`]: its remark already answers).
///
/// zgc wr (2026-10-23, `zgc-weak-reference-held-by-a-dead-finalizable-is-cleared-before-finalize-FIXED-20261023`):
/// default ON on ZGC too. HotSpot 25's ZGC keeps that referent through
/// `finalize()` as Serial does, and every ZGC collection of this VM runs
/// through this door's pause, so the same probe answers it
/// (`GciI15RfUndiscoveredRefProbe old|young` on the default, concurrent,
/// generational and `--compatible` arms: HotSpot's stdout).
pub(in crate::runtime::interpreter::gc_and_alloc) fn stw_undiscovered_refs_strong_on(shared: &SharedVm) -> bool {
    if !shared.mem.heap.is_g1() {
        true    } else {
        stw_undiscovered_refs_strong_enabled()    }}

/// gci i48/rf48: the full-collection half of [`stw_probe_engaged`] and of
/// the door's carry ([`g1_carry_door_undiscovered_refs`]): G1, a
/// full-collection door, and the opt-in switch (read last).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_gc_keeps_undiscovered(shared: &SharedVm, door: GcDoor) -> bool {
    g1_full_gc_keeps_undiscovered_rule(
        shared.mem.heap.is_g1(),
        door,
        &g1_full_gc_keeps_undiscovered_refs_enabled,
    )
}

/// The rule of [`g1_full_gc_keeps_undiscovered`], apart. Pure but for the
/// switch reader, which is read only when the other two hold.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_gc_keeps_undiscovered_rule(
    g1: bool,
    door: GcDoor,
    switch_on: &dyn Fn() -> bool,
) -> bool {
    g1 && g1_full_collection_door(door) && switch_on()
}

/// The most objects one probe walk visits ([`stw_probe_closure`]). A walk
/// that would pass it gives up (the probe is an over-approximation helper,
/// never a soundness input: an abandoned probe only loses HotSpot's keep of an
/// undiscovered reference's referent).
///
/// gcj j2/x1 (`gcj-j2x1-wave-j2-default-flips-...` item (ii), row
/// `Gcj3X2StwProbeBudgetProbe big`): the pre-collection walk
/// ([`StwUndiscoveredProbe::before_collection_if`]) no longer gives up
/// WHOLE. It walks its roots round-robin ([`stw_probe_walk_fair`]) and keeps
/// the pairs found before the budget: a live finalizable holding a graph past
/// the budget no longer costs a small dead one its keep.
pub(in crate::runtime::interpreter::gc_and_alloc) const STW_PROBE_WALK_BUDGET: usize = 1 << 20;

/// gci i16/fr: every object reachable from `roots` as the stop-the-world
/// marker will see it AFTER the pre-collection pass (a nulled slot 0 reads
/// null, every other field is an edge), plus, for each visited address
/// `referent_of` answers, that referent as an extra edge. Reference arrays are
/// walked element-wise, primitive arrays have no edges. `None` when the walk
/// would visit more than `budget` objects.
///
/// Only reads the heap, inside the pause: before the collection (from the
/// registered finalizable objects, none of them freed yet) or after it (from
/// the resurrected ones, at their post-collection addresses).
pub(in crate::runtime::interpreter::gc_and_alloc) fn stw_probe_closure(
    shared: &SharedVm,
    roots: &[usize],
    referent_of: &dyn Fn(usize) -> Option<usize>,
    budget: usize,
) -> Option<Vec<usize>> {
    stw_probe_closure_admitting(shared, roots, referent_of, &|_| true, budget)
}

/// gcj j3/x2 (`gcj-j2x1-wave-j2-default-flips-...` item (ii)): the census of
/// [`StwUndiscoveredProbe::before_collection_if`]'s walk, which the j2 flip
/// put on every Generational pause that nulled a slot with a finalizable
/// registered. Process totals, observability only (no decision reads them):
/// walks run, walks abandoned past [`STW_PROBE_WALK_BUDGET`] (before: silent,
/// the pause then ran without the probe and lost HotSpot's keep), objects
/// visited (an abandoned walk counts its budget), and walk nanoseconds.
pub(in crate::runtime::interpreter::gc_and_alloc) static STW_PROBE_WALKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::gc_and_alloc) static STW_PROBE_ABANDONED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::gc_and_alloc) static STW_PROBE_VISITED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::gc_and_alloc) static STW_PROBE_WALK_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// gcj j3/x2: the pre-collection probe walk with the census counted, and one
/// `[watchref] stw-probe:` line per walk under `CRATONVM_DBG_WATCHREF=1`
/// (the cost row's measurement and the abandonment's positive control).
///
/// gcj j2/x1: the walk is [`stw_probe_walk_fair`]'s -- what it visited, and
/// whether it finished inside `budget` (`false`: counted as abandoned, the
/// visited prefix still returned). A finished walk visits exactly
/// [`stw_probe_closure`]'s set.
pub(in crate::runtime::interpreter::gc_and_alloc) fn stw_probe_closure_counted(
    shared: &SharedVm,
    roots: &[usize],
    referent_of: &dyn Fn(usize) -> Option<usize>,
    budget: usize,
) -> (Vec<usize>, bool) {
    use std::sync::atomic::Ordering::Relaxed;
    let start = std::time::Instant::now();
    let (walk, complete) = stw_probe_walk_fair(shared, roots, referent_of, budget);
    let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let visited = walk.len() as u64;
    STW_PROBE_WALKS.fetch_add(1, Relaxed);
    STW_PROBE_VISITED.fetch_add(visited, Relaxed);
    STW_PROBE_WALK_NS.fetch_add(ns, Relaxed);
    if !complete {
        STW_PROBE_ABANDONED.fetch_add(1, Relaxed);
    }
    if crate::runtime::env_cache::dbg_watchref() {
        eprintln!(
            "[watchref] stw-probe: walk roots={} visited={} us={} abandoned={} total={}",
            roots.len(),
            visited,
            ns / 1000,
            !complete,
            STW_PROBE_ABANDONED.load(Relaxed),
        );
    }
    (walk, complete)
}

/// gcj j2/x1 (`gcj-j2x1-wave-j2-default-flips-...` item (ii)): the probe walk
/// of [`stw_probe_closure`] from `roots`, taken ROUND-ROBIN -- one visited
/// object per root per turn, over one shared visited set -- so a root whose
/// closure is small finishes early whatever the others reach. Returns the
/// visited objects and whether the walk finished inside `budget`; past it the
/// walk stops and the prefix is returned. Before, one walk drained the roots
/// depth-first and gave up WHOLE past the budget, so one live finalizable
/// holding a large graph cost every dead one its pairs
/// (`Gcj3X2StwProbeBudgetProbe big`). A finished walk visits the same set.
pub(in crate::runtime::interpreter::gc_and_alloc) fn stw_probe_walk_fair(
    shared: &SharedVm,
    roots: &[usize],
    referent_of: &dyn Fn(usize) -> Option<usize>,
    budget: usize,
) -> (Vec<usize>, bool) {
    let heap = &shared.mem.heap;
    // addr-keyed: local to one pause's probe walk (dropped before the pause ends).
    let mut seen: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut visited: Vec<usize> = Vec::new();
    // One work stack per root whose walk is still going.
    let mut stacks: Vec<Vec<usize>> = roots.iter().map(|&r| vec![r]).collect();
    let mut turn = 0usize;
    while !stacks.is_empty() {
        if turn >= stacks.len() {
            turn = 0;
        }
        let work = &mut stacks[turn];
        while let Some(addr) = work.pop() {
            if addr == 0 || !seen.insert(addr) {
                continue;
            }
            let Some(obj) = heap.is_heap_addr(addr) else {
                continue;
            };
            if visited.len() >= budget {
                return (visited, false);
            }
            visited.push(addr);
            stw_probe_push_edges(shared, obj, addr, referent_of, &|_| true, work);
            break;
        }
        if work.is_empty() {
            stacks.swap_remove(turn);
        } else {
            turn += 1;
        }
    }
    (visited, true)
}

/// The edges of the probe walk out of the visited `obj` at `addr`, onto
/// `work`: `referent_of(addr)`, then every reference field / element (read
/// without marking), each only when `admit` accepts it. Shared by
/// [`stw_probe_closure_admitting`] and [`stw_probe_walk_fair`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn stw_probe_push_edges(
    shared: &SharedVm,
    obj: ObjectRef,
    addr: usize,
    referent_of: &dyn Fn(usize) -> Option<usize>,
    admit: &dyn Fn(usize) -> bool,
    work: &mut Vec<usize>,
) {
    let heap = &shared.mem.heap;
    if let Some(referent) = referent_of(addr) {
        if admit(referent) {
            work.push(referent);
        }
    }
    let mut push = |v: Value| {
        if let Value::Object(Some(child)) = v {
            let child = child.as_ptr() as usize;
            if admit(child) {
                work.push(child);
            }
        }
    };
    match heap.kind_of(obj) {
        crate::memory::heap::ObjectKind::Array => {
            if heap.array_element_type(obj) != Some(ArrayElementType::Reference) {
                return;
            }
            // zt t32: non-marking reads, as the reference pass's: the
            // closing collection's mark is still up here under the color
            // flip, and a marking read kept every object this walk
            // reaches (a dead finalizable object's whole reach) alive.
            for i in 0..heap.array_length(obj) {
                if let Ok(v) = heap.get_array_element_weak(obj, i) {
                    push(v);
                }
            }
        }
        crate::memory::heap::ObjectKind::Object => {
            for i in 0..heap.num_fields(obj) {
                push(heap.get_field_weak(obj, i));
            }
        }
        _ => {}
    }
}

/// gci i17/fz: [`stw_probe_closure`] that follows an edge (a field, an
/// element or a `referent_of` answer) only to an address `admit` accepts;
/// `roots` are always visited. `admit` sees each reached address once per
/// edge, before the visited-set check.
pub(in crate::runtime::interpreter::gc_and_alloc) fn stw_probe_closure_admitting(
    shared: &SharedVm,
    roots: &[usize],
    referent_of: &dyn Fn(usize) -> Option<usize>,
    admit: &dyn Fn(usize) -> bool,
    budget: usize,
) -> Option<Vec<usize>> {
    let heap = &shared.mem.heap;
    // addr-keyed: local to one pause's probe walk (dropped before the pause ends).
    let mut seen: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut visited: Vec<usize> = Vec::new();
    let mut work: Vec<usize> = roots.to_vec();
    while let Some(addr) = work.pop() {
        if addr == 0 || !seen.insert(addr) {
            continue;
        }
        let Some(obj) = heap.is_heap_addr(addr) else {
            continue;
        };
        if visited.len() >= budget {
            return None;
        }
        visited.push(addr);
        stw_probe_push_edges(shared, obj, addr, referent_of, admit, &mut work);
    }
    Some(visited)
}

/// gci i16/fr (G-9, the stop-the-world half; `CRATONVM_STW_UNDISCOVERED_REFS_STRONG`):
/// HotSpot's order for a `Reference` that only a dead finalizable object
/// reaches, in a stop-the-world pause.
///
/// HotSpot discovers a `Reference` only when its strong marking reaches it;
/// `process_discovered_references` then disables discovery, so a `Reference`
/// first reached by the FinalReference keep-alive phase (through a dead
/// finalizable object) is an ordinary object: its referent is traced strongly
/// and kept, and a LATER collection discovers it. Here the pre-collection pass
/// nulls every active referent slot before the mark, so the collection's
/// finalizer resurrection reaches the `Reference` but not its referent, which
/// is freed, and the round then clears the reference (rows `rf15_young_*`).
///
/// The probe, in three steps (all inside the pause):
///
/// 1. [`Self::before_collection`] (after the pre-collection pass): walk from
///    the registered finalizable objects; every `Reference` with a nulled
///    slot it reaches is a pair `(Reference, referent)`, and the walk goes on
///    through the referent (a kept referent can reach another such
///    `Reference`). The referents are added to the collection's resurrection
///    candidates, so the collection keeps any it did not mark strongly
///    instead of freeing it.
/// 2. [`Self::after_collection`]: the candidates the collection resurrected
///    are taken out of the dead-finalizer list (they are not finalizable);
///    a resurrected referent no dead finalizable object's closure reaches is
///    "candidate only". Both lists go to the processor
///    (`ReferenceProcessor::note_stw_undiscovered_probe`).
/// 3. The round (`gc/src/reference.rs`): a pair whose `Reference` the
///    collection marked only through its finalizer resurrection is left alone
///    (HotSpot never discovered it; the restore loop writes its referent
///    back), and a candidate-only referent no such pair keeps is unmarked for
///    the whole round (its discovered references are cleared / enqueued as
///    without the probe).
///
/// Inactive (the pause exactly as before, no allocation) when the switch is
/// off, no finalizable object is registered, `CRATONVM_WEAKREF_CLEAR=0` (no
/// slot was nulled), `CRATONVM_DBG_NO_REFPROC`, or the walk finds no pair
/// (gcj j2/x1: inside [`STW_PROBE_WALK_BUDGET`]; a walk past it keeps the
/// pairs it found).
pub(in crate::runtime::interpreter::gc_and_alloc) struct StwUndiscoveredProbe {
    /// `(Reference, referent)`, PRE-collection addresses.
    pub(in crate::runtime::interpreter::gc_and_alloc) pairs: Vec<(usize, usize)>,
    /// The collection's resurrection candidates: the door's finalizable
    /// roots plus [`Self::referents`], sorted; empty when `referents` is.
    pub(in crate::runtime::interpreter::gc_and_alloc) candidates: Vec<usize>,
    /// The probe referents that are not themselves finalizable roots,
    /// PRE-collection addresses, sorted.
    pub(in crate::runtime::interpreter::gc_and_alloc) referents: Vec<usize>,
}

impl StwUndiscoveredProbe {
    pub(in crate::runtime::interpreter::gc_and_alloc) fn inactive() -> Self {
        Self {
            pairs: Vec::new(),
            candidates: Vec::new(),
            referents: Vec::new(),
        }
    }

    /// Step 1. `fin_roots` is the door's `finalizable_roots` snapshot; the
    /// walk starts from the processor's registered (not yet enqueued)
    /// finalizable objects only -- one already queued for `finalize()` is
    /// strongly reachable in HotSpot (from the `Finalizer` queue), so a
    /// `Reference` it reaches is discovered there.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn before_collection(shared: &SharedVm, fin_roots: &[usize]) -> Self {
        Self::before_collection_if(shared, fin_roots, &stw_undiscovered_refs_strong_enabled)
    }

    /// gci i48/rf48: [`Self::before_collection`] engaged by `engaged` (read
    /// last, only when a finalizable is registered) instead of the opt-in
    /// switch alone; the door passes [`stw_probe_engaged`], which also
    /// engages it on the pauses of a G1 full collection.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn before_collection_if(
        shared: &SharedVm,
        fin_roots: &[usize],
        engaged: &dyn Fn() -> bool,
    ) -> Self {
        if fin_roots.is_empty() || !weakref_clear_enabled() || no_refproc() || !engaged() {
            return Self::inactive();
        }
        let (registered, mut nulled_rows) = {
            let rp = shared.mem.ref_processor.lock();
            let registered = rp.finalizer_referent_addresses();
            let mut rows = rp.weak_phantom_active_pairs();
            rows.extend(rp.soft_pre_nulled_active_pairs());
            rows.retain(|&(r, t)| r != 0 && t != 0 && rp.was_nulled_by_pre_gc_pass(r));
            (registered, rows)
        };
        if registered.is_empty() || nulled_rows.is_empty() {
            return Self::inactive();
        }
        nulled_rows.sort_unstable();
        nulled_rows.dedup_by_key(|row| row.0);
        let referent_of = |addr: usize| -> Option<usize> {
            nulled_rows
                .binary_search_by_key(&addr, |row| row.0)
                .ok()
                .map(|i| nulled_rows[i].1)
        };
        // gcj j3/x2: counted (the walk's cost and its abandonment were silent).
        // gcj j2/x1: a walk past the budget keeps the pairs it found -- each
        // is a correct keep (a `Reference` some registered finalizable
        // reaches, with its pre-nulled referent); only the unfound ones lose
        // HotSpot's keep, as every pair did before.
        let (visited, _complete) =
            stw_probe_closure_counted(shared, &registered, &referent_of, STW_PROBE_WALK_BUDGET);
        let pairs: Vec<(usize, usize)> = visited
            .iter()
            .filter_map(|&a| referent_of(a).map(|t| (a, t)))
            .collect();
        if pairs.is_empty() {
            return Self::inactive();
        }
        let mut referents: Vec<usize> = pairs
            .iter()
            .map(|&(_, t)| t)
            .filter(|t| fin_roots.binary_search(t).is_err())
            .collect();
        referents.sort_unstable();
        referents.dedup();
        let candidates = if referents.is_empty() {
            Vec::new()
        } else {
            let mut c = fin_roots.to_vec();
            c.extend_from_slice(&referents);
            c.sort_unstable();
            c.dedup();
            c
        };
        Self {
            pairs,
            candidates,
            referents,
        }
    }

    /// What the door hands the collection as its resurrection candidates
    /// (and the depth-1 note as the candidate list): `fin_roots` itself
    /// unless the probe added referents.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn collector_candidates<'a>(&'a self, fin_roots: &'a [usize]) -> &'a [usize] {
        if self.referents.is_empty() {
            fin_roots
        } else {
            &self.candidates
        }
    }

    /// Step 2. `dead` is the collection's dead-finalizer list (POST-collection
    /// addresses); returns it without the probe referents, which are not
    /// finalizable and must never reach `enqueue_resurrected_finalizers`.
    /// Notes the probe to the processor for this pause's round.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn after_collection(
        &self,
        shared: &SharedVm,
        pointer_map: &cratonvm_types::PointerMap,
        dead: Vec<usize>,
    ) -> Vec<usize> {
        if self.pairs.is_empty() {
            return dead;
        }
        // (POST, PRE) for each probe referent, sorted by POST.
        let mut probe_post: Vec<(usize, usize)> = self
            .referents
            .iter()
            .map(|&y| (pointer_map.get(&y).copied().unwrap_or(y), y))
            .collect();
        probe_post.sort_unstable();
        // gci i17/fz (i16/fr's residual): the Generational collection
        // published every candidate it was handed, these referents included,
        // to the next concurrent remark (`exchange_concurrent_finalizers`);
        // they are not finalizable, so take them back in this pause. A remark
        // that ran first retained a dead one for its cycle. No-op on G1 / ZGC
        // and without a concurrent cycle.
        if !self.referents.is_empty() {
            let posts: Vec<usize> = probe_post.iter().map(|p| p.0).collect();
            shared
                .mem
                .heap
                .retract_concurrent_finalizer_candidates(&posts);
        }
        let pre_of_post = |a: usize| -> Option<usize> {
            probe_post
                .binary_search_by_key(&a, |p| p.0)
                .ok()
                .map(|i| probe_post[i].1)
        };
        let mut real_dead: Vec<usize> = Vec::with_capacity(dead.len());
        let mut resurrected_probe: Vec<(usize, usize)> = Vec::new();
        for a in dead {
            match pre_of_post(a) {
                Some(pre) => resurrected_probe.push((a, pre)),
                None => real_dead.push(a),
            }
        }
        // A resurrected referent a dead finalizable object's closure also
        // reaches is kept by HotSpot's keep-alive phase whatever its
        // `Reference`: not candidate-only. An abandoned walk names none
        // (over-retention for one round, never an early clear).
        let candidates_only: Vec<usize> = if resurrected_probe.is_empty() {
            Vec::new()
        } else {
            match stw_probe_closure(shared, &real_dead, &|_| None, STW_PROBE_WALK_BUDGET) {
                Some(mut reached) => {
                    reached.sort_unstable();
                    let seeds: Vec<(usize, usize)> = resurrected_probe
                        .iter()
                        .copied()
                        .filter(|&(post, _)| reached.binary_search(&post).is_err())
                        .collect();
                    let mut only: Vec<usize> = seeds.iter().map(|&(_, pre)| pre).collect();
                    // gci i17/fz (i16/fr's residual): and what only they reach.
                    only.extend(self.dead_closure_of_candidates_only(
                        shared,
                        pointer_map,
                        &seeds,
                        &reached,
                    ));
                    only
                }
                None => Vec::new(),
            }
        };
        if !no_refproc() {
            shared
                .mem
                .ref_processor
                .lock()
                .note_stw_undiscovered_probe(&self.pairs, &candidates_only);
        }
        real_dead
    }

    /// gci i17/fz (i16/fr's residual: a phantom reference to an object
    /// reachable only THROUGH a candidate-only referent was enqueued one
    /// collection late, because only the referent itself was unmarked for the
    /// round). Answers the PRE-collection addresses of the objects the
    /// collection marked only through its finalizer resurrection
    /// (`ReferenceProcessor::resurrected_noted_this_cycle`) that only the
    /// candidate-only referents `seeds` (`(POST, PRE)`) reach: not reached
    /// from a real dead finalizable object (`reached`, sorted POST), and not
    /// from a referent an undiscovered pair keeps (the round keeps those; their
    /// closure is HotSpot's keep-alive closure). The round then treats these as
    /// unmarked too, so a weak / soft reference to one is cleared and a
    /// phantom one is enqueued in this round, as HotSpot does for an object
    /// only the cleared referent reached. Both walks stay inside the
    /// resurrection set, so a strongly marked object is never named. Empty
    /// (fr's behaviour: the seeds alone) when nothing was noted, no seed is
    /// left, or a walk passes [`STW_PROBE_WALK_BUDGET`].
    pub(in crate::runtime::interpreter::gc_and_alloc) fn dead_closure_of_candidates_only(
        &self,
        shared: &SharedVm,
        pointer_map: &cratonvm_types::PointerMap,
        seeds: &[(usize, usize)],
        reached: &[usize],
    ) -> Vec<usize> {
        if seeds.is_empty() {
            return Vec::new();
        }
        // addr-keyed: POST -> PRE of this pause's resurrection set, local to the probe (dropped before the pause ends).
        let (res_post, kept_post): (rustc_hash::FxHashMap<usize, usize>, Vec<usize>) = {
            let rp = shared.mem.ref_processor.lock();
            let noted = rp.resurrected_noted_this_cycle();
            if noted.is_empty() {
                return Vec::new();
            }
            // `resolve_stw_undiscovered_probe`'s rule: a pair whose
            // `Reference` is in the resurrection set keeps its referent.
            let kept: Vec<usize> = self
                .pairs
                .iter()
                .filter(|&&(r, _)| r != 0 && noted.contains(&r))
                .map(|&(_, y)| pointer_map.get(&y).copied().unwrap_or(y))
                .collect();
            let map = noted
                .iter()
                .map(|&pre| (pointer_map.get(&pre).copied().unwrap_or(pre), pre))
                .collect();
            (map, kept)
        };
        let in_res = |a: usize| -> bool { res_post.contains_key(&a) };
        let Some(mut protect) = stw_probe_closure_admitting(
            shared,
            &kept_post,
            &|_| None,
            &in_res,
            STW_PROBE_WALK_BUDGET,
        ) else {
            return Vec::new();
        };
        protect.sort_unstable();
        let shielded = |a: usize| -> bool {
            reached.binary_search(&a).is_ok() || protect.binary_search(&a).is_ok()
        };
        let roots: Vec<usize> = seeds
            .iter()
            .map(|&(post, _)| post)
            .filter(|&post| !shielded(post))
            .collect();
        if roots.is_empty() {
            return Vec::new();
        }
        let admit = |a: usize| -> bool { in_res(a) && !shielded(a) };
        let Some(dead) =
            stw_probe_closure_admitting(shared, &roots, &|_| None, &admit, STW_PROBE_WALK_BUDGET)
        else {
            return Vec::new();
        };
        // The seeds come back too (already in the caller's list; the
        // processor's set absorbs the repeat).
        dead.into_iter()
            .filter_map(|post| res_post.get(&post).copied())
            .collect()
    }
}

/// gci i15/rf (`gci-i14gq-g1-reference-processing-and-finalization-review-findings-FIXED-20261016`
/// G-7): the HotSpot `Indents` depth of this stop-the-world pause's
/// `gc,phases,ref` block ([`crate::runtime::unified_logging::xlog_reference_lines_at`]).
/// HotSpot G1 prints an evacuation pause's block inside `Post Evacuate
/// Collection Set` (`print_all_references(2, false)`: 2); Serial's
/// collections, a G1 Full GC and the remarks print it at the top level (0).
/// A pause folded into a G1 full collection's one report
/// (`CRATONVM_G1_FULL_GC_ONE_REPORT`, the calling thread owns the open scope)
/// reports as HotSpot's `Pause Full`, so it takes 0 too. Called inside the
/// pause, only when `-Xlog` takes the block.
pub(in crate::runtime::interpreter::gc_and_alloc) fn reference_block_indent_levels(shared: &SharedVm) -> usize {
    let folded_into_full = shared
        .mem
        .alloc_ladder
        .g1_full_report
        .try_lock()
        .is_some_and(|scope| {
            (*scope)
                .as_ref()
                .is_some_and(|s| s.owner == std::thread::current().id())
        });
    reference_block_indent_levels_rule(shared.mem.heap.is_g1(), folded_into_full)
}

/// [`reference_block_indent_levels`], pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn reference_block_indent_levels_rule(g1: bool, folded_into_full: bool) -> usize {
    if g1 && !folded_into_full {
        2
    } else {
        0
    }
}







/// gci i27/gr: the G1 arm of [`process_references_after_gc`]'s survival
/// verdict for the young regions the pause kept in place.
///
/// A G1 pause that cannot evacuate a young region (a JIT pin, a JNI critical
/// pin, an evacuation failure) leaves every object in it below the cursor,
/// dead or alive, and `VmHeap::watched_pre_gc_addr_survived` answers an
/// unmoved address by region membership: every weak / phantom referent in
/// such a region read live, so `new WeakReference<>(new Object())` held by a
/// compiled frame kept its dead referent on every young pause. This asks G1
/// which of the active weak / phantom rows' `Reference` objects and referents
/// that the pause neither moved nor identity-mapped NOTHING holds after the
/// pause (`G1Collector::pinned_young_referents_unheld`: the pause's own
/// liveness vocabulary -- roots, JIT pin words, every young region, the
/// candidate regions' remembered-set sources, the overlay rows), and answers
/// those dead. A registered or queued finalizable object is never a WEAK
/// candidate (its verdict decides `finalize()`, and its resurrection is the
/// collector's); since gci i31/gq the registered ones are judged in a list of
/// their own and resurrected here when dead (see below).
///
/// Empty -- the region-granular answer stands -- off G1, with
/// `CRATONVM_G1_PINNED_REFERENT_VERDICT=0`, on a pause that kept no young
/// region, and whenever G1 declines (an open marking cycle, an unparsable
/// region, the walk budget). `CRATONVM_DBG_G1_PINNED_REFERENTS=1` prints one
/// `[g1-pinned-referents]` line per armed pause.
///
/// gci i28/wt (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`):
/// taken ONCE per pause by `run_collection_pause` (it used to be taken inside
/// `process_references_after_gc`, for the reference rows only) and carried on
/// the pause's `addr_keyed::RelocationTargets`, so every in-pause weak
/// consumer reads it: the reference round's `is_marked`, the JNI weak-global
/// sweep and every side-table sweep through `InPlaceVerdict::survived`. The
/// candidates are the reference rows' (only with `CRATONVM_WEAKREF_CLEAR` on
/// -- no pre-collection pass nulled a slot otherwise -- and the round not
/// switched off by `CRATONVM_DBG_NO_REFPROC`, as before) plus every uncleared
/// JNI weak global's referent (`jni::weak_global_referent_addrs`), minus the
/// mapped and the finalizable addresses ([`g1_pinned_referent_candidates`]).
/// G1's verdict for one address does not depend on which other addresses are
/// offered (`gci_i28_wt_a_candidate_s_verdict_does_not_depend_on_the_others`),
/// so the union answers each consumer exactly as a verdict of its own would.
/// A finalizable referent stays out: the weak global's phantom strength keeps
/// it, as the collection resurrects it.
///
/// gci i31/gq (`gci-i30rp-g1-dead-finalizable-in-a-kept-young-region-is-not-finalized-at-the-pause-FIXED-20261016`,
/// default ON, `CRATONVM_G1_PINNED_REFERENT_FINALIZABLES=0` kills): the
/// REGISTERED finalizables (not queued, not mapped) are offered too, as a
/// list of their own ([`g1_kept_young_finalizable_candidates`]). One the
/// verdict finds unheld -- or held only by an object this collection
/// resurrected (`finalizer_only`) -- is dead as a collection-set one would
/// be: it is taken OUT of the returned unheld set (every in-place verdict
/// keeps it, as the collection keeps a resurrected object), noted as
/// resurrected for the round (weak references to it clear, final / phantom
/// keep it) and handed back in `kept_dead_finalizables` (its unchanged
/// address), which the door appends to the pause's `dead_finalizers` before
/// `enqueue_resurrected_finalizers`. Nothing moves and its region stays
/// kept, so `finalize()` runs against valid memory: the region's outgoing
/// edges are remembered-set sources of this pause (every cross-region store
/// is recorded), so what it names in the collection set was copied and its
/// slots rewritten whether it was live or not.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_pinned_young_referents_unheld(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    cycle_roots: &[ObjectRef],
    kept_dead_finalizables: &mut Vec<usize>,
    // addr-keyed: this pause's PRE-collection addresses, consumed by its own epilogue.
) -> rustc_hash::FxHashSet<usize> {
    if !shared.mem.heap.is_g1()
        || !shared.mem.heap.g1_pinned_referent_verdict_armed()
    {
        return rustc_hash::FxHashSet::default();
    }
    let pairs = if weakref_clear_enabled() && !no_refproc() {
        shared.mem.ref_processor.lock().weak_phantom_active_pairs()
    } else {
        Vec::new()
    };
    let jni_weak = crate::native::jni::weak_global_referent_addrs(shared);
    // The registered finalizables AND the queued ones (`finalizable_roots`, the
    // pause's own resurrection input: the pause keeps a queued object alive,
    // so it is held here too).
    let finalizable = finalizable_roots(shared);
    let mut candidates = g1_pinned_referent_candidates(&pairs, &finalizable, pointer_map);
    g1_pinned_referent_extra_candidates(&mut candidates, &jni_weak, &finalizable, pointer_map);
    // gci i30/rp (the i28/wt residual "side-table owners not offered"): the
    // owners of this VM's rows in the weak side tables the epilogue sweeps
    // with `in_place.survived` (Locale, TLS, lock keys), same screen. A
    // candidate's verdict does not depend on the others (i28/wt), so the
    // reference rows and the JNI weak globals are answered as before.
    let mut table_owners: Vec<usize> = Vec::new();
    {
        cratonvm_native_builtins::gc_weak_row_owners(shared.vm_identity, &mut table_owners);
        g1_pinned_referent_extra_candidates(
            &mut candidates,
            &table_owners,
            &finalizable,
            pointer_map,
        );
    }
    // gci i31/gq: the registered (not queued) finalizables the pause neither
    // moved nor identity-mapped, behind their own kill. Disjoint from the
    // lists above (those drop every finalizable), and a candidate's verdict
    // does not depend on the others (i28/wt), so the weak consumers are
    // answered as before.
    let finalizable_candidates = {
        let registered = shared.mem.ref_processor.lock().finalizer_referent_addresses();
        let queued = shared.mem.finalizer_thread.pending_addresses();
        g1_kept_young_finalizable_candidates(&registered, &queued, pointer_map)
    };
    if !finalizable_candidates.is_empty() {
        candidates.extend(finalizable_candidates.iter().copied());
        candidates.sort_unstable();
        candidates.dedup();
    }
    if candidates.is_empty() {
        return rustc_hash::FxHashSet::default();
    }
    let roots: Vec<usize> = cycle_roots.iter().map(|r| r.as_ptr() as usize).collect();
    let verdict = shared
        .mem
        .heap
        .g1_pinned_young_referents_unheld(&candidates, &roots);
    if false {
        match &verdict {
            Ok(v) => eprintln!(
                "[g1-pinned-referents] rows={} jni_weak={} table_owners={} offered={} in_kept_young={} confirmed={} unheld={} finalizer_only={} regions_walked={} bytes_walked={}",
                pairs.len(),
                jni_weak.len(),
                table_owners.len(),
                candidates.len(),
                v.candidates,
                v.confirmed,
                v.unheld.len(),
                v.finalizer_only.len(),
                v.regions_walked,
                v.bytes_walked,
            ),
            Err(why) => eprintln!(
                "[g1-pinned-referents] rows={} jni_weak={} offered={} declined={why:?}",
                pairs.len(),
                jni_weak.len(),
                candidates.len(),
            ),
        }
    }
    match verdict {
        Ok(mut v) => {
            // gci i30/rp (`i27-gr-report.md` review item 4): a weak referent in
            // a kept region that only an object this pause resurrected for
            // `finalize()` holds is finalizer-reachable: the round clears its
            // weak references (HotSpot), the in-place verdicts keep it.
            let mut noted = {
                g1_finalizer_only_weak_referents(&pairs, &v.finalizer_only)
            };
            // gci i31/gq: the dead registered finalizables of the kept
            // regions leave the unheld set (they are resurrected, not freed)
            // and go to the door's queue; the round reads them resurrected.
            let dead = g1_kept_young_dead_finalizables(&finalizable_candidates, &mut v);
            if false {
                eprintln!(
                    "[g1-pinned-referents] finalizables offered={} dead={}",
                    finalizable_candidates.len(),
                    dead.len(),
                );
            }
            if !dead.is_empty() {
                if !no_refproc() {
                    noted.extend(dead.iter().copied());
                    // gci i32/ec (`i31-gq-report.md` residual): the weak
                    // referents only those finalizables reach -- children the
                    // pause copied out of the collection set included -- are
                    // finalizer-reachable too (HotSpot clears them).
                    {
                        noted.extend(g1_kept_finalizable_private_weak_referents(
                            shared,
                            &dead,
                            &roots,
                            pointer_map,
                            &pairs,
                        ));
                    }
                }
                kept_dead_finalizables.extend(dead);
            }
            if !noted.is_empty() {
                shared
                    .mem
                    .ref_processor
                    .lock()
                    .note_resurrected_finalizables(&noted);
            }
            v.unheld
        }
        Err(_) => rustc_hash::FxHashSet::default(),
    }
}



/// gci i31/gq: the finalizable candidates of [`g1_pinned_young_referents_unheld`],
/// pure: every non-null REGISTERED finalizable (`registered`, the processor's
/// never-enqueued rows) that is not waiting in the finalizer queue (`queued`:
/// already the queue's, a resurrection input of every pause) and that the
/// pause did not map (a key moved, or is an identity survivor or a
/// resurrected collection-set object); ascending, each once. G1 itself keeps
/// only those below the cursor of a young region.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_kept_young_finalizable_candidates(
    registered: &[usize],
    queued: &[usize],
    pointer_map: &cratonvm_types::PointerMap,
) -> Vec<usize> {
    let mut queued: Vec<usize> = queued.to_vec();
    queued.sort_unstable();
    let mut out: Vec<usize> = registered
        .iter()
        .copied()
        .filter(|&a| {
            a != 0 && !pointer_map.contains_key(&a) && queued.binary_search(&a).is_err()
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// gci i31/gq: of `offered` (ascending, from
/// [`g1_kept_young_finalizable_candidates`]), the ones the verdict found
/// unheld or held only by this collection's resurrected objects (HotSpot
/// enqueues a `FinalReference` whose referent is not strongly reachable, a
/// finalizer-reachable one included); each is removed from both sets of the
/// verdict -- it is kept alive for `finalize()`, so no in-place consumer may
/// read it dead, and it is not a weak referent the round should note.
/// Ascending, each once.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_kept_young_dead_finalizables(
    offered: &[usize],
    verdict: &mut cratonvm_gc::g1::G1PinnedReferentVerdict,
) -> Vec<usize> {
    let mut dead: Vec<usize> = Vec::new();
    for &a in offered {
        let unheld = verdict.unheld.remove(&a);
        let finalizer_only = verdict.finalizer_only.remove(&a);
        if unheld || finalizer_only {
            dead.push(a);
        }
    }
    dead
}



/// gci i32/ec: the weak referents of `pairs` (PRE-collection addresses) that
/// only the dead kept-region finalizables `dead` reach, through children the
/// pause copied out of the collection set or left in a kept region
/// (`G1Collector::kept_finalizables_private_closure`, POST-collection
/// addresses, mapped back by [`g1_closure_pre_collection_addrs`]); a
/// `Reference` object of a row is left out as i30/rp leaves it out
/// ([`g1_finalizer_only_weak_referents`]). The caller notes them resurrected
/// for the round: soft / weak references to them clear, final / phantom keep
/// them. Nothing is freed or moved by the answer (the objects stay where the
/// pause put them, kept alive by the queued finalizables). Empty with no
/// rows, no dead finalizable, or when G1 declines.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_kept_finalizable_private_weak_referents(
    shared: &SharedVm,
    dead: &[usize],
    roots: &[usize],
    pointer_map: &cratonvm_types::PointerMap,
    pairs: &[(usize, usize)],
) -> Vec<usize> {
    if dead.is_empty() || pairs.is_empty() {
        return Vec::new();
    }
    let closure = shared
        .mem
        .heap
        .g1_kept_finalizables_private_closure(dead, roots);
    if false {
        match &closure {
            Ok(c) => eprintln!(
                "[g1-pinned-referents] finalizable closure seeds={} members={} finalizer_only={} walks={} complete={} bytes_walked={} held_by_slot={} held_by_root={} held_by_pin={} held_by_overlay={}",
                dead.len(),
                c.closure,
                c.finalizer_only.len(),
                c.walks,
                c.complete,
                c.bytes_walked,
                c.held_by_slot,
                c.held_by_root,
                c.held_by_pin,
                c.held_by_overlay,
            ),
            Err(why) => eprintln!(
                "[g1-pinned-referents] finalizable closure seeds={} declined={why:?}",
                dead.len(),
            ),
        }
    }
    // gci i41/hc: `CRATONVM_G1_DBG_CLOSURE_ROOT_ENTRIES` (diagnostic, reads
    // only): which root entry holds each member of the closure.
    if let Ok(c) = &closure {
        crate::memory::roots::g1_dbg_closure_root_entries(shared, c, dead, roots, pointer_map);
    }
    match closure {
        Ok(c) => {
            let pre = g1_closure_pre_collection_addrs(&c.finalizer_only, pointer_map);
            g1_finalizer_only_weak_referents(pairs, &pre)
        }
        Err(_) => Vec::new(),
    }
}

/// gci i32/ec: `post` (POST-collection addresses) as PRE-collection
/// addresses: a key of `pointer_map` whose value is in `post` (a copy the
/// pause made, or an identity entry), and every `post` address no entry maps
/// to as itself (an object of a kept region, which did not move). Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_closure_pre_collection_addrs(
    // addr-keyed: this pause's POST-collection addresses, consumed by its own epilogue.
    post: &rustc_hash::FxHashSet<usize>,
    pointer_map: &cratonvm_types::PointerMap,
    // addr-keyed: this pause's PRE-collection addresses, consumed by its own round.
) -> rustc_hash::FxHashSet<usize> {
    // addr-keyed: local to one pause.
    let mut pre: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    if post.is_empty() {
        return pre;
    }
    // addr-keyed: local to one pause.
    let mut mapped: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    for (&from, &to) in pointer_map.iter() {
        if post.contains(&to) {
            pre.insert(from);
            mapped.insert(to);
        }
    }
    for &a in post {
        if !mapped.contains(&a) {
            pre.insert(a);
        }
    }
    pre
}


/// gci i30/rp: the referents of `pairs` (the active weak / phantom rows the
/// verdict was offered, `(Reference, referent)`) that G1 found held only by
/// the pause's resurrected objects (`finalizer_only`), minus any address that
/// is itself a `Reference` object of a row (its row's own pruning reads the
/// same set; a `Reference` reachable only through a finalizable is left as
/// before); ascending, each once. Empty when `pairs` is (the rows are gathered
/// only with `CRATONVM_WEAKREF_CLEAR` on and the round not switched off by
/// `CRATONVM_DBG_NO_REFPROC`, so nothing is noted for a round that will not
/// run). Noting a phantom-only referent is harmless: the round's final and
/// phantom phases read the raw mark, which keeps it.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_finalizer_only_weak_referents(
    pairs: &[(usize, usize)],
    // addr-keyed: this pause's PRE-collection addresses, consumed by its own round.
    finalizer_only: &rustc_hash::FxHashSet<usize>,
) -> Vec<usize> {
    if finalizer_only.is_empty() || pairs.is_empty() {
        return Vec::new();
    }
    // addr-keyed: local to one pause.
    let references: rustc_hash::FxHashSet<usize> = pairs.iter().map(|&(r, _)| r).collect();
    let mut out: Vec<usize> = pairs
        .iter()
        .map(|&(_, referent)| referent)
        .filter(|a| *a != 0 && finalizer_only.contains(a) && !references.contains(a))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}


/// gci i27/gr: the candidates [`g1_pinned_young_referents_unheld`] offers G1,
/// pure: every non-null `Reference` object and referent of an active weak /
/// phantom row that the pause did not map (a map key moved, or is an identity
/// survivor, and is answered by the map), minus every registered or queued
/// finalizable object; ascending, each once. G1 itself keeps only those in a
/// young region.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_pinned_referent_candidates(
    pairs: &[(usize, usize)],
    finalizable: &[usize],
    pointer_map: &cratonvm_types::PointerMap,
) -> Vec<usize> {
    // addr-keyed: local to one pause.
    let finalizable: rustc_hash::FxHashSet<usize> = finalizable.iter().copied().collect();
    let mut out: Vec<usize> = pairs
        .iter()
        .flat_map(|&(reference, referent)| [reference, referent])
        .filter(|&a| a != 0 && !pointer_map.contains_key(&a) && !finalizable.contains(&a))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// gci i28/wt: add the addresses of another weak consumer of the pause (the
/// JNI weak globals' referents) to `candidates`, with the same screen as
/// [`g1_pinned_referent_candidates`] (non-null, not a key of the map, not a
/// registered or queued finalizable object); ascending, each once.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_pinned_referent_extra_candidates(
    candidates: &mut Vec<usize>,
    extra: &[usize],
    finalizable: &[usize],
    pointer_map: &cratonvm_types::PointerMap,
) {
    if extra.is_empty() {
        return;
    }
    // addr-keyed: local to one pause.
    let finalizable: rustc_hash::FxHashSet<usize> = finalizable.iter().copied().collect();
    let offered = |a: usize| a != 0 && !pointer_map.contains_key(&a) && !finalizable.contains(&a);
    candidates.extend(extra.iter().copied().filter(|&a| offered(a)));
    candidates.sort_unstable();
    candidates.dedup();
}

/// zt t31/zc11 (program page 1c step 2c, `CRATONVM_ZGC_CONCURRENT_REF_ROUND`):
/// does this collection run its reference round after the release? The heap's
/// answer (`ZgcRealHeap::reference_round_may_follow_release`: the switch, a
/// parked sweep, step 2a's hidden slots, single-generation) and the VM's: no
/// pointer moved, the reference subsystem on, and a `ReferenceQueue` class
/// with a `lock` field, so every enqueue takes the Reference Handler hand-off
/// (the direct-write enqueue is a stopped-world protocol). Asked inside the
/// pause, after the collection.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_reference_round_after_release(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
) -> bool {
    if !shared.mem.heap.zgc_reference_round_may_follow_release() {
        return false;
    }
    let queue_takes_the_hand_off = {
        let cm = shared.classes.class_manager.read();
        cm.find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue")
            .and_then(|cid| {
                crate::vm::vm_exec::resolve_field_index_in_hierarchy(cid, "lock", cm.class_store())
            })
            .is_some()
    };
    if !pointer_map.is_empty() || no_refproc() || !weakref_clear_enabled() || !queue_takes_the_hand_off
    {
        shared.mem.heap.zgc_note_reference_round_kept_by_vm();
        return false;
    }
    true
}


/// zt t31/zc11: the parked sweep's install, held for a reference round after
/// the release; dropping it unreleased (an unwind out of the round) releases
/// it with no round booked, so no runner waits for good.
pub(in crate::runtime::interpreter::gc_and_alloc) struct ZgcSweepInstallHold<'a>(pub &'a SharedVm);

impl ZgcSweepInstallHold<'_> {
    pub(in crate::runtime::interpreter::gc_and_alloc) fn release(self, round_us: u64) {
        self.0.mem.heap.zgc_release_sweep_install_hold(round_us);
        std::mem::forget(self);
    }
}

impl Drop for ZgcSweepInstallHold<'_> {
    fn drop(&mut self) {
        self.0.mem.heap.zgc_release_sweep_install_hold(0);
    }
}

/// Process weak/soft references after a GC cycle.
/// Calls the ReferenceProcessor, nulls referent fields of cleared references,
/// and relocates ref processor addresses using the pointer map.
///
/// zt t31/zc11: three steps -- [`process_references_head`] (the loader /
/// class-unload reconciliation and the diagnostic short-circuit),
/// [`reference_round_prep`] (the values read before the locks, then the
/// locks) and [`reference_round`] (the round). A ZGC collection that runs the
/// round after its release (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`) calls the
/// three itself, the last two after the rest of its pause.

pub(in crate::runtime::interpreter) fn absorb_zgc_discovered_weak_phantom(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
) {
    let discovered = shared.mem.heap.take_zgc_discovered_weak_phantom();
    if discovered.is_empty() {
        return;
    }
    let mut slot_cache: std::collections::HashMap<ClassId, (usize, usize)> = Default::default();
    let mut resolved = Vec::with_capacity(discovered.len());
    let mut post_queues = std::collections::HashSet::new();
    for (pre_base, cid_raw, kind_code, pre_ref_opt, _referent_alive) in discovered {
        let Some(pre_ref) = pre_ref_opt else { continue; };
        let live_base = pointer_map.get(&pre_base).copied().unwrap_or(pre_base);
        let cid = ClassId::new(cid_raw);
        let (_ref_slot, queue_slot) = *slot_cache.entry(cid).or_insert_with(|| {
            let r = crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, cid, "referent").unwrap_or(0);
            let q = crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, cid, "queue").unwrap_or(1);
            (r, q)
        });
        let obj_ref = unsafe { ObjectRef::from_raw(live_base as *mut u8) };
        let queue = match shared.mem.heap.get_field_as(obj_ref, queue_slot, b'L') {
            Value::Object(Some(q)) => {
                let qa = q.as_ptr() as usize;
                post_queues.insert(qa);
                Some(qa)
            }
            _ => None,
        };
        let ref_type = if kind_code == 1 {
            cratonvm_gc::reference::ReferenceType::Weak
        } else {
            cratonvm_gc::reference::ReferenceType::Phantom
        };
        let runs_cleaner = kind_code == 3;
        resolved.push((ref_type, pre_base, cid_raw, runs_cleaner, pre_ref, queue));
    }
    let mut rev_queue: std::collections::HashMap<usize, usize> = Default::default();
    if !post_queues.is_empty() && !pointer_map.is_empty() {
        for (&pre, &post) in pointer_map {
            if post_queues.contains(&post) {
                rev_queue.insert(post, pre);
            }
        }
    }
    if !resolved.is_empty() {
        let mut rp = shared.mem.ref_processor.lock();
        for (ref_type, pre_base, cid_raw, runs_cleaner, pre_ref, queue) in resolved {
            let pre_queue = queue.map(|q| rev_queue.get(&q).copied().unwrap_or(q));
            rp.register_discovered_weak_phantom(
                ref_type,
                pre_base,
                cid_raw,
                runs_cleaner,
                pre_ref,
                pre_queue,
            );
        }
    }
}

pub(in crate::runtime::interpreter) fn process_references_after_gc(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    cycle_roots: &[ObjectRef],
    // addr-keyed: borrowed ZGC cycle verdicts, queried with original loader bases before remap.
    reconcile_exact_results: Option<&std::collections::HashMap<usize, bool>>,
    // gci i22/dr: the pause's one relocation-destination set over `pointer_map`
    // (`addr_keyed::RelocationTargets::for_pause`, built by `run_collection_pause`).
    pause_relocations: &crate::memory::addr_keyed::RelocationTargets<'_>,
) {
    if process_references_head(
        shared,
        pointer_map,
        cycle_roots,
        reconcile_exact_results,
        pause_relocations,
    ) {
        absorb_zgc_discovered_weak_phantom(shared, pointer_map);
        let prep = reference_round_prep(shared, pointer_map);
        reference_round(shared, pointer_map, pause_relocations, prep, false);
    }
}

/// zt t31/zc11: the head of [`process_references_after_gc`] -- the
/// defining-loader / class-unload reconciliation and the mirror and overlay
/// audits, which stay inside the pause in every mode. `false` when the
/// diagnostic `CRATONVM_DBG_NO_REFPROC` short-circuit ran (no round follows).
pub(in crate::runtime::interpreter) fn process_references_head(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    cycle_roots: &[ObjectRef],
    // addr-keyed: borrowed ZGC cycle verdicts, queried with original loader bases before remap.
    reconcile_exact_results: Option<&std::collections::HashMap<usize, bool>>,
    pause_relocations: &crate::memory::addr_keyed::RelocationTargets<'_>,
) -> bool {
    debug_assert!(
        pause_relocations.lent_for(pointer_map),
        "process_references_after_gc: the relocation set was built over another pointer map"
    );
    // gc-common w16-x: one capture of the Generational young free list for
    // every weak side-table verdict below, instead of one per row. Nothing in
    // this function allocates on the heap, so it cannot go stale before the
    // last sweep; the free-list probe itself is taken lazily, at the first
    // sweep that needs it (after the unload transaction). See
    // `addr_keyed::InPlaceVerdict`.
    //
    // gcd d10/r: taken with this pause's relocations, and captured here (it
    // used to be taken after `gen_stw_layout_census`) so every liveness
    // verdict of this function shares its lazily-built destination set. An
    // address a slide moved another survivor ONTO is not the object that was
    // there: without the screen a dead `ClassLoader`, mirror, overlay owner,
    // proxy `Method`, weak-row owner, `Reference` or referent whose base a ZGC
    // slide or a Generational compaction re-issued read as LIVE (the address
    // parses as an object -- the survivor's) and kept its row, which then
    // answered for the survivor. `addr_keyed::RelocationTargets`; a no-op on
    // G1, on a non-moving pause and on a Generational cycle that reclaimed no
    // old storage.
    //
    // gci i22/dr (P2/dr): over the pause's ONE destination set, which the JNI
    // weak sweep and `update_all_roots` then reuse -- built at most once per
    // pause instead of once per verdict; same answers as `capture_after`.
    let in_place = crate::memory::addr_keyed::InPlaceVerdict::capture_with(
        &shared.mem.heap,
        pause_relocations,
    );
    // An address that is NOT a key of the map (a key moved away, and every
    // verdict below answers it through the map) and that this pause relocated
    // another object onto: whatever lived there before the pause is dead.
    let claimed = |addr: usize| -> bool {
        !pointer_map.contains_key(&addr) && in_place.claimed_by_relocation(addr)
    };
    // HIB-CV-24 (Manifestation B): reconcile the defining-loader side-table with
    // this collection. A user `ClassLoader` the application no longer references
    // is now collectable (it is not GC-rooted under `CRATONVM_LOADER_UNLOAD`);
    // prune its stale side-table entry and remap survivors using the SAME
    // survivor predicate the reference processor uses below. Done BEFORE the
    // diagnostic `no_refproc` short-circuit so the side-table never holds a
    // dangling/stale ObjectRef after a collection, independent of that switch.
    {
        let is_marked = |addr: usize| -> bool {
            pointer_map.contains_key(&addr)
                || (shared.mem.heap.is_addr_live(addr) && !claimed(addr))
        };
        let dead_class_hints = cratonvm_native_builtins::classloader::gc_reconcile_defining_loaders(
            &shared.loader_identity,
            &is_marked,
            pointer_map,
            reconcile_exact_results,
        );
        // The reconcile rebuilt this VM's loader pins from the defining-loader
        // table, which has no lambda-proxy rows; re-add them (a host that died
        // this cycle has no row left, so its proxies get none).
        crate::runtime::invokedynamic::repin_lambda_proxy_loaders(shared.vm_identity);
        let unload = crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints);
        if unload.classes_unloaded != 0 {
            tracing::debug!(
                loaders = unload.loaders_unloaded,
                classes = unload.classes_unloaded,
                jit_entries = unload.jit_entries_retired,
                "class-loader metadata unloaded"
            );
        }
        // gcd d2/g: a stop-the-world collection that reclaimed old storage
        // takes the retained-layout census too (nothing to do unless a
        // concurrent remark retained a layout: one thread-local read, and one
        // leaf-mutex read after an old reclamation).
        gen_stw_layout_census(shared);
        g1_stw_layout_census(shared);
        // (`in_place`, the one in-place verdict every weak side-table sweep
        // below takes, is captured at the top of this function -- gcd d10/r.)
        // gc-common w11-d: the Locale subtag side tables are weak (their keys
        // are no longer roots), so the rows of Locales this collection did not
        // keep are dropped here, on every stop-the-world cycle including the
        // non-moving ones, before any allocation can reuse a freed address.
        // Keys are still PRE-remap (`update_all_roots` re-keys the survivors
        // later): a moved key is in the pointer map, an unmoved one is judged
        // by the `addr_keyed::survived_in_place` verdict every VM-driven
        // address-keyed sweep uses. See `native-builtins` `gc_sweep_locale_rows`.
        cratonvm_native_builtins::gc_sweep_locale_rows(shared.vm_identity, &|addr| {
            pointer_map.contains_key(&addr) || in_place.survived(addr)
        });
        // gc-common w12-a: the TLS rows owned by an `SSLEngine` / `SSLSession`
        // / `SSLParameters` / real `HttpURLConnection` go with their owner --
        // same verdict, same moment (the `tls` root row re-addresses the
        // survivors in `update_all_roots`). See `native-builtins`
        // `gc_sweep_tls_rows`.
        cratonvm_native_builtins::gc_sweep_tls_rows(shared.vm_identity, &|addr| {
            pointer_map.contains_key(&addr) || in_place.survived(addr)
        });
        // gc-common w14-d: the JUL rows keyed on a Logger / Handler /
        // FileHandler, and the Tomcat JULI registries keyed on a context class
        // loader, go with their key -- same verdict, same moment (the
        // `logmanager` root row re-keys the survivors in `update_all_roots`).
        // See `native-builtins` `logmanager::gc_sweep_logging_rows`.
        cratonvm_native_builtins::logmanager::gc_sweep_logging_rows(shared.vm_identity, &|addr| {
            pointer_map.contains_key(&addr) || in_place.survived(addr)
        });
        // gc-common w14-c: the lock-key registry's slots go with their object
        // (or become a tombstone while an `SSLContext` key may still be
        // named), same verdict, after the TLS rows; the survivors are then
        // re-addressed through `pointer_map`, which is the registry's only
        // remap. See `native-builtins` `gc_sweep_and_remap_lock_keys`.
        let swept = cratonvm_native_builtins::gc_sweep_and_remap_lock_keys(
            shared.vm_identity,
            &|addr| pointer_map.contains_key(&addr) || in_place.survived(addr),
            pointer_map,
        );
        // A dead user loader's defined-`Package` rows go with its freed weak
        // key; the VM-less sweep cannot reach this VM's memo (interpreter
        // round i5 wave 105, lane L5).
        if swept > 0 {
            cratonvm_native_builtins::lang_class::forget_dead_loader_defined_packages(
                &shared.loader_identity,
            );
        }
        // gc-common w14-e: native-io's weak side tables (the
        // `BufferedOutputStream` / `PipedOutputStream` side buffers) -- same
        // verdict, same moment. Owners are PRE-remap here; the sweep
        // re-addresses a moved owner itself through `pointer_map`, so no
        // `native_roots.rs` remap row is needed. See `native-io`
        // `gc_sweep_io_side_tables`.
        cratonvm_native_io::gc_sweep_io_side_tables(shared.vm_identity, pointer_map, &|addr| {
            in_place.survived(addr)
        });
        // gc-common w15-f: the `com.sun.net.httpserver` link rows (server,
        // executor, authenticator and attribute-map roots keyed on their
        // owner) go with their owner -- same verdict, same moment. Owners are
        // PRE-remap here; the sweep re-addresses a moved owner itself through
        // `pointer_map`, like the native-io sweep above. See `native-builtins`
        // `phases_late::net_channels::gc_sweep_http_link_rows`.
        cratonvm_native_builtins::phases_late::net_channels::gc_sweep_http_link_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // gc-common w16-a: the `java.net` socket rows (a dead unclosed
        // `Socket` has its stream closed), the `SSLSessionContext` carrier
        // roots and bindings, the `SSLServerSocket` option delegates and the
        // `HttpClient` shutdown rows go with their owner -- same verdict,
        // same moment. Owners are PRE-remap here; the sweep re-addresses a
        // moved owner itself through `pointer_map`, like the two sweeps above.
        // See `native-builtins` `net_phase_e::gc_sweep_net_socket_rows`.
        cratonvm_native_builtins::net_phase_e::gc_sweep_net_socket_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // gc-common w17-c: the `java.util.zip` side tables (an unclosed
        // stream's decoded payload, a Spring Boot `FileAccess` snapshot, an
        // un-`end()`ed Deflater/Inflater) and native-io's `ZipFile`/`JarFile`
        // tables (an unclosed archive is released, as HotSpot's `ZipFile`
        // cleaner does) go with their owner -- same verdict, same moment.
        // Owners are PRE-remap here; the sweep re-addresses a moved owner
        // itself through `pointer_map`, like the sweeps above. See
        // `native-builtins` `phases_late::zip_streams::gc_sweep_zip_rows`.
        cratonvm_native_builtins::phases_late::zip_streams::gc_sweep_zip_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // gc-common w18-g: the synthetic `SubmissionPublisher`
        // closed-exception rows go with their publisher (its Throwable's
        // global root is released by that VM's next `closeExceptionally`).
        // Owners are PRE-remap here; the sweep re-addresses a moved owner
        // itself through `pointer_map`, like the sweeps above. See
        // `native-builtins` `phases_late::concurrent::gc_sweep_submission_publisher_rows`.
        cratonvm_native_builtins::phases_late::concurrent::gc_sweep_submission_publisher_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // gc-common w20-g: the Undertow `HeaderMap` object rows (and their
        // header entries) and `Undertow$Builder` configuration rows go with
        // their owner -- same verdict, same moment. Owners are PRE-remap here;
        // the sweep re-addresses a moved owner itself through `pointer_map`,
        // like the sweeps above. See `native-builtins`
        // `wildfly_undertow::gc_sweep_undertow_rows`.
        cratonvm_native_builtins::wildfly_undertow::gc_sweep_undertow_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // Prune dead entries from the overlay-backed-collection side-tables
        // (LinkedList / LinkedHashMap / TreeMap / TreeSet — `roots.rs` step 17
        // / `native_collections::gc_scan_collection_overlay_roots`). This
        // function existed but was never called from anywhere in the tree
        // (confirmed: `gc_prune_dead_collection_overlays` had zero call
        // sites) — a collection whose OWN object becomes genuinely
        // unreachable left its registry entry (and every element it ever
        // held) permanently behind, since nothing ever shrank these tables.
        // Wiring this in is a real, independent, safe fix (verified: prunes
        // ~1000/1470 stale entries per GC cycle in the Tomcat suite) with no
        // change to rooting behavior — it only removes bookkeeping for
        // collections `is_marked` already agrees are dead.
        //
        // NOTE: this does NOT fully close
        // `defaultinstancemanager-classunloading-count-mismatch-FIXED.md`.
        // `roots.rs` step 17 itself has a separate, deeper bug this session
        // found but did not fix: `gc_scan_collection_overlay_roots` roots
        // EVERY element of EVERY overlay-backed collection unconditionally,
        // with no gate on whether the backing collection is reachable. A
        // scratch `List<StackMapFrame>` the JDT compiler uses transiently
        // during JSP compilation gets its elements force-rooted this way;
        // forward-tracing from that illegitimate root walks back through the
        // compiler's real field references into the evicted JSP's
        // `JspServletWrapper` and its `ClassLoader`, keeping the whole
        // cluster permanently, artificially reachable — confirmed via a
        // root-membership closure check (21 direct hits, all contributed by
        // step 17, not by any other root source). A full fix needs the same
        // conditional-rooting + mark-time-propagation treatment this session
        // gave `class_mirrors` (see `cratonvm_types::mirror_pin`), but scoped
        // to every overlay table instead of just one cache — a materially
        // larger, higher-risk change than fit in this session; left for a
        // dedicated follow-up.
        //
        // Called here (not `update_all_roots`/gc.rs, where the existing
        // remap call `gc_update_collection_overlay_refs` lives) for the same
        // reason `reconcile_class_mirrors` is here and not there:
        // `update_all_roots` early-returns when `pointer_map` is empty (the
        // common case for the non-moving JIT-active sweep), so it would
        // never run for that path. `is_marked` already handles PRE-GC
        // addresses correctly for both the moving and non-moving cases
        // (pointer_map lookup for moved survivors, `is_addr_live` for
        // not-moved ones) — the same pattern `gc_reconcile_defining_loaders`
        // above already relies on — so pruning here with pre-remap addresses
        // is correct; the later `gc_update_collection_overlay_refs` remap
        // pass in `update_all_roots` only touches whatever prune left behind.
        //
        // MEASURED FALSE-DEAD (2026-08-01): the paragraph above is wrong about
        // which addresses reach here. `run_non_moving_young_cycle` calls
        // `remap_external_roots` INSIDE the collector, so by this point
        // `slot.last_ptr` is already POST-GC — and `is_marked`'s first arm
        // (`pointer_map.contains_key`) only ever matches a PRE-GC address, so
        // for anything that moved the whole verdict falls to `is_addr_live`.
        // `ROverlaySystemGcStress` shows that verdict killing 10 LIVE
        // collections in one cycle (`dead_keys=10`, immediately followed by a
        // populated `TreeMap` reading back as `size 0`). A false-dead here is
        // silent data loss with no dangling pointer for any verifier to find;
        // a false-live is one cycle of retained bookkeeping. The two are not
        // symmetric and this predicate treats them as if they were.
        //
        // `CRATONVM_DBG_OVERLAY_PRUNE=1` reports every address this is about
        // to condemn, with the evidence, so the arm responsible is a fact and
        // not another inference.
        let prune_dbg =
            None::<std::ffi::OsString>.is_some();
        // Only THIS VM's owners may be condemned. The overlay registry is
        // process-wide, and `is_marked` answers `false` for every address
        // outside this heap, so without a screen each VM's collection deleted
        // every other VM's live overlay-backed collections.
        //
        // The screen is by VM (gc-common w12-c): `prune_external_roots_for_vm`
        // hands the native-collection provider this VM's identity, and it asks
        // `is_live` only about the object-key slots this VM created. That
        // covers every backend, Generational included
        // (`common-w10g-generational-overlay-prune-has-no-ownership-screen`).
        //
        // The span screen (gc-common w8-d) stays for a provider that has no
        // VM-scoped callbacks. On G1 and ZGC `conservative_addr_span` is the
        // one arena, fixed for the heap's lifetime, so it is exact. NOT on
        // Generational: its span is the envelope of arenas a young `grow` can
        // move during this very collection, so a dead owner's pre-GC address
        // could fall outside it and never be pruned (a leak, and its elements
        // stay rooted).
        let owned_here = if shared.mem.heap.is_generational() {
            None
        } else {
            shared.mem.heap.conservative_addr_span()
        };
        // Both screens are applied by `prune_external_roots_for_vm`: another
        // VM's slot, or an owner outside `owned_here`, is never handed to this
        // predicate.
        //
        // G1 follows owner edges itself and reports the CSet owners it found
        // dead: their rows must go whatever `is_marked` says, because an
        // evacuation failure keeps a region (or a drain reuses a freed one)
        // and the dead owner's address then reads as live while its elements,
        // never evacuated, dangle (`gc/src/g1_overlay.rs`). Empty elsewhere.
        let condemned = shared.mem.heap.take_overlay_condemned_owners();
        let is_live_for_prune = |addr: usize| -> bool {
            let live = !condemned.contains(&addr) && is_marked(addr);
            if prune_dbg && !live {
                let in_heap = shared.mem.heap.is_heap_addr(addr).is_some();
                let words: [u64; 3] = if in_heap {
                    // SAFETY: `is_heap_addr` placed `addr` in a mapped region.
                    unsafe { std::ptr::read_unaligned(addr as *const [u64; 3]) }
                } else {
                    [0; 3]
                };
                let (old_alloc, young_surv, region) = shared.mem.heap.liveness_arms(addr);
                eprintln!(
                    "[overlay-prune] CONDEMNED 0x{addr:x} in_heap={in_heap} region={region} \
                     in_pointer_map={} old_gen_allocated={old_alloc} young_survivor={young_surv} \
                     w0=0x{:016x} w1=0x{:016x} w2=0x{:016x}",
                    pointer_map.contains_key(&addr),
                    words[0],
                    words[1],
                    words[2],
                );
            }
            live
        };
        if prune_dbg {
            // `collection_count()` is minor_gc_count + major_gc_count
            // (`VmHeap::collection_count`), and a single STW cycle that also
            // promotes (`major_ran` in `gen_heap.rs`) bumps BOTH counters
            // before returning to this one call site — so a jump of 2 here
            // between successive prunes is one physical collection, not a
            // skipped one. See `WORKER-5-NOTE-10` N2: this line is the
            // assertion that nomination asked for.
            eprintln!(
                "[overlay-prune] collection_count={} (pre-prune)",
                shared.mem.heap.collection_count()
            );
        }
        cratonvm_gc::external_roots::prune_external_roots_for_vm(
            shared.vm_identity,
            owned_here,
            &is_live_for_prune,
        );
        // Opt-in audit of what prune left behind: an overlay ref pointing at a
        // reclaimed object. Here rather than in `update_all_roots` because that
        // early-returns on an empty `pointer_map`, i.e. never runs on the
        // non-moving sweep — the path a `System.gc()` takes.
        crate::memory::gc::audit_overlay_refs(shared);
        // Companion reconciliation for the class-mirror cache — see
        // `memory::gc::reconcile_class_mirrors` / `roots.rs` step 6. Same
        // "before the no_refproc short-circuit" rationale: the cache must
        // never hold a stale ObjectRef after a collection, independent of
        // that diagnostic switch.
        crate::memory::gc::reconcile_class_mirrors(shared, &is_marked, Some(cycle_roots));
        // Rebuild the mirror_pin registry the GC marker consults (gen_heap.rs)
        // from the now-pruned class_mirrors + just-remapped defining-loader
        // side-table, so the marker sees current addresses next cycle.
        crate::memory::gc::rebuild_mirror_pins(shared, pointer_map);
    }

    // bc math-ec 0x4 (CRATONVM_DBG_NO_REFPROC): subsystem-level exclusion
    // switch — skip ALL post-GC reference processing (clears, enqueues,
    // finalizer/cleaner submissions). If the corruption persists with this
    // set, the whole reference subsystem is exonerated in one experiment;
    // if it stops, the writer is in here. Diagnostic only (weak refs never
    // clear; memory grows).
    if no_refproc() {
        // gci i42/rp (`gci-i42rp-the-no-refproc-exclusion-arm-nulls-every-weak-referent-FIXED-20261016`):
        // the raw addresses this pass would relocate below still move with a
        // moving pause. The finalizer queue keeps filling on this arm
        // (`enqueue_resurrected_finalizers` and `finalizable_roots` take no
        // gate) and is a root of every later pause; left at its pre-move
        // addresses it handed the collector from-space words. Relocation
        // only -- no clear, enqueue, prune or submission, as the switch says.
        shared.mem.cleaner_thread.update_after_gc(pointer_map);
        shared.mem.finalizer_thread.update_after_gc(pointer_map);
        shared.mem.ref_processor.lock().update_after_gc(pointer_map);
        return false;
    }
    true
}

/// zt t31/zc11 (program page 1c step 2c, `CRATONVM_ZGC_CONCURRENT_REF_ROUND`):
/// what [`reference_round`] reads before it takes its locks, and the two
/// guards it runs under, taken in rank order (the class manager, L10, before
/// the reference processor, L7). [`process_references_after_gc`] takes them
/// and runs the round at once; a ZGC collection that runs the round after the
/// release takes them as the last thing in its pause and holds them through
/// the release, so no row joins the processor and no class is defined between
/// the mark and the round.
pub(in crate::runtime::interpreter) struct ReferenceRoundPrep<'a> {
    pub(in crate::runtime::interpreter::gc_and_alloc) free_mb: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) refproc_prepass: bool,
    pub(in crate::runtime::interpreter::gc_and_alloc) reference_next_slot: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) enqueued_sentinel: Option<ObjectRef>,
    pub(in crate::runtime::interpreter::gc_and_alloc) class_manager:
        crate::runtime::lock_order::OrderedPlRwLockReadGuard<'a, crate::classloading::ClassManager>,
    pub(in crate::runtime::interpreter::gc_and_alloc) reference_cid: Option<ClassId>,
    pub(in crate::runtime::interpreter::gc_and_alloc) queue_cid: Option<ClassId>,
    pub(in crate::runtime::interpreter::gc_and_alloc) ref_proc: crate::runtime::lock_order::OrderedPlMutexGuard<'a, cratonvm_gc::ReferenceProcessor>,
}

/// zt t31/zc11: [`ReferenceRoundPrep`] for `pointer_map`'s collection.
pub(in crate::runtime::interpreter) fn reference_round_prep<'a>(
    shared: &'a SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
) -> ReferenceRoundPrep<'a> {
    // Pressure input to the SoftReference LRU policy (HotSpot's
    // `LRUMaxHeapPolicy`: clear when `idle_ms > SoftRefLRUPolicyMSPerMB *
    // free_heap_mb`). This used to be a hardcoded `64`, which pinned the
    // threshold at a constant 64 seconds of idleness no matter how full the
    // heap was, so the policy could not respond to memory pressure at all.
    // `soft_ref_policy_free_mb()` returns whole megabytes of *allocatable*
    // headroom (min of young/eden and old-gen promotion room, capped by
    // whole-heap headroom, rounded down so a sub-MB remainder reads as 0 =
    // maximum pressure) — the right figure under the non-moving fragmenting
    // collector that actually runs by default, where unused bytes and
    // obtainable bytes diverge. Read *before* taking the reference-processor
    // lock: the accessor reaches into the heap's own generation stats, and
    // there is no reason to nest those acquisitions.
    // See `refs-metaspace-unloading.md` §2/§R1.
    // gch h2/jr: through `soft_lru_free_mb` (the same figure under
    // `CRATONVM_SOFTREF_HOTSPOT_FREE=0`; HotSpot's `MaxHeapSize - used` by
    // default, on every collector since gci i48/zl2).
    let free_mb = crate::memory::ref_discovery::soft_lru_free_mb(shared);
    // Read before the reference-processor lock. See the call site below for
    // what it gates and why it now covers every backend (it used to ask the
    // heap `is_generational()`).
    let refproc_prepass = !no_refproc_prepass();
    // ClassManager is rank L10 and the reference processor is L7, so resolve
    // the JDK field before acquiring the lower-ranked processor lock.
    let reference_next_slot = gc_reference_next_slot(shared);
    // gc-common w2-f: what the enqueue loop stores in a real-layout
    // `Reference.queue`. See `gc_enqueued_sentinel`.
    let enqueued_sentinel = gc_enqueued_sentinel(shared, pointer_map);
    // Same rank rule, one step further: the shape guard below asks the class
    // hierarchy a question per entry, so its read guard is taken HERE — L10
    // before L7 — and held across the two loops rather than reacquired inside
    // them. Nothing between this line and the `drop` after the enqueue loop
    // touches the ClassManager for writing; the mirror/loader reconciliation
    // that does is all above, before this point.
    let class_manager = shared.classes.class_manager.read();
    let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
    let queue_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue");
    let ref_proc = shared.mem.ref_processor.lock();
    ReferenceRoundPrep {
        free_mb,
        refproc_prepass,
        reference_next_slot,
        enqueued_sentinel,
        class_manager,
        reference_cid,
        queue_cid,
        ref_proc,
    }
}

