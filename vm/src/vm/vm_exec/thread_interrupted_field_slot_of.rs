#[allow(unused_imports)]
use super::*;

/// The `interrupted` field slot to write in `thread_obj`, resolved BEFORE any
/// per-thread interrupt lock is taken (nio21 wave 5 L1: that lock is a leaf).
///
/// `memo` (interpreter round i3 waves 76/77, lane L6:
/// `CRATONVM_INTERRUPT_FIELD_MEMO`, and `CRATONVM_INTERRUPT_ONE_LOOKUP` for
/// `Thread.interrupt`): through the per-VM thread-layout memo
/// ([`thread_field_slot`]: the `interrupted` field `java.lang.Thread`
/// declares, without the class-manager lock once warm). The same slot: the
/// memo binds to `java.lang.Thread` and answers a subclass from the `Thread`
/// above it. With `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS=0` the memo
/// answers a subclass BY NAME on the subclass, which finds a subclass's own
/// `interrupted` field; the field `Thread` declares is the one the JDK reads,
/// so that case (and `memo == false`) keeps the by-name resolution on
/// `java/lang/Thread` ([`thread_interrupted_field_slot`]). Merged at nio21
/// from dev's `set_thread_interrupted_field_through_memo`.
pub(in crate::vm::vm_exec) fn thread_interrupted_field_slot_of(
    shared: &SharedVm,
    thread_obj: ObjectRef,
    memo: bool,
) -> Option<usize> {
    if memo && shared.threads.thread_layout_cache.declared_fields_on() {
        let heap = &shared.mem.heap;
        let (obj, validated) = heap.load_and_forward_checked(thread_obj);
        if !validated {
            return None;
        }
        return thread_field_slot(shared, heap.class_id_of(obj), "interrupted");
    }
    thread_interrupted_field_slot(shared)
}

/// The `java/lang/Thread.interrupted` field slot, or `None` before `Thread`
/// is loaded (or for a `Thread` without the field). Takes the class-manager
/// read lock, so a caller that holds the per-thread interrupt lock
/// (`ThreadRegistry::lock_interrupt`) resolves it BEFORE taking that lock.
///
/// Resolved against `java/lang/Thread` itself, not the receiver's class: a
/// `Thread` subclass that declares its own `interrupted` field would
/// otherwise be written through the subclass's slot.
pub(in crate::vm::vm_exec) fn thread_interrupted_field_slot(shared: &SharedVm) -> Option<usize> {
    let cm = shared.classes.class_manager.read();
    cm.get_loaded_class_id("java/lang/Thread")
        .and_then(|thread_class| {
            resolve_field_index_in_hierarchy(thread_class, "interrupted", &cm.class_store)
        })
}

/// Store `value` into `thread_obj`'s `interrupted` field at a slot resolved by
/// [`thread_interrupted_field_slot`]. Takes no lock and allocates nothing.
pub(in crate::vm::vm_exec) fn store_thread_interrupted_field(
    shared: &SharedVm,
    thread_obj: ObjectRef,
    slot: Option<usize>,
    value: bool,
) {
    let Some(slot) = slot else {
        return;
    };
    let heap = &shared.mem.heap;
    let (obj, validated) = heap.load_and_forward_checked(thread_obj);
    if !validated {
        return;
    }
    heap.set_field(obj, slot, Value::Int(i32::from(value)));
}

/// Consume the current thread's interrupt: clear `flag` (the `JvmThread`'s
/// `interrupted`, shared with its registry entry) and, when it was set, the
/// Java `Thread.interrupted` field of `mirror`. Returns whether it was set.
///
/// nio21 wave 5 L1 (page
/// `r21-interrupt-field-and-flag-can-split-on-a-consume-race-FIXED-20261009.md`):
/// the two clears run under the per-thread interrupt lock that
/// `NativeContextImpl::thread_interrupt` takes around its two raises, so a
/// racing `interrupt()` lands entirely before the consume (and is consumed) or
/// entirely after it (and is kept in both records). Without the lock
/// (`CRATONVM_INTERRUPT_LOCK=0`, the pre-wave-5 order) consume(flag := 0),
/// interrupt(field := 1, flag := 1), consume(field := 0) leaves the flag set
/// and the field clear. The uninterrupted path is one atomic load and takes
/// no lock.
pub(crate) fn consume_thread_interrupt(
    shared: &SharedVm,
    flag: &std::sync::atomic::AtomicBool,
    mirror: Option<ObjectRef>,
) -> bool {
    use std::sync::atomic::Ordering;
    if !flag.load(Ordering::Acquire) {
        return false;
    }
    let memo = shared.threads.thread_registry.interrupt_field_memo();
    let slot = mirror.and_then(|m| thread_interrupted_field_slot_of(shared, m, memo));
    let _guard = shared.threads.thread_registry.lock_interrupt(flag);
    let was = flag.swap(false, Ordering::AcqRel);
    if was {
        if let Some(mirror) = mirror {
            store_thread_interrupted_field(shared, mirror, slot, false);
        }
    }
    was
}

/// Raise thread `tid`'s interrupt in both records: the Java `Thread.interrupted`
/// field of `thread_obj` and the registry's flag (the `Arc` the target's
/// `JvmThread` shares), under the target's interrupt lock (nio21 wave 5 L1;
/// see [`consume_thread_interrupt`]). The flag `Arc` and the field slot are
/// resolved before the lock, which is a leaf. With no registry entry only the
/// field is written, as before. The wake-ups are the caller's.
pub(crate) fn raise_thread_interrupt(
    shared: &SharedVm,
    tid: Option<ThreadId>,
    thread_obj: ObjectRef,
) {
    let registry = &shared.threads.thread_registry;
    let interrupt_flag = tid.and_then(|tid| registry.get_interrupted_flag(tid));
    let memo = registry.interrupt_field_memo() || registry.interrupt_one_lookup();
    let slot = thread_interrupted_field_slot_of(shared, thread_obj, memo);
    let _guard = interrupt_flag
        .as_deref()
        .and_then(|flag| registry.lock_interrupt(flag));
    store_thread_interrupted_field(shared, thread_obj, slot, true);
    // SeqCst (round 14 wave 5, lane monitor2, RV5-1; the store
    // `ThreadRegistry::set_interrupted` made): with the `fence(SeqCst)` in
    // `MonitorTable::wake_waiter_for_interrupt` and the SeqCst enrolment
    // re-check in `Monitor::wait`.
    if let Some(flag) = interrupt_flag.as_deref() {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Clear the Java `Thread.interrupted` field of `mirror` when the VM flag
/// reads clear, both under the per-thread interrupt lock: for a consume that
/// found the flag already cleared (the `Object.wait` exit woken by an
/// interrupt another consume already took). Never clears the field of a
/// thread whose flag a racing `interrupt()` has just raised again.
pub(in crate::vm::vm_exec) fn sync_thread_interrupted_field_to_clear_flag(
    shared: &SharedVm,
    flag: &std::sync::atomic::AtomicBool,
    mirror: Option<ObjectRef>,
) {
    let Some(mirror) = mirror else {
        return;
    };
    let memo = shared.threads.thread_registry.interrupt_field_memo();
    let slot = thread_interrupted_field_slot_of(shared, mirror, memo);
    let _guard = shared.threads.thread_registry.lock_interrupt(flag);
    if !flag.load(std::sync::atomic::Ordering::Acquire) {
        store_thread_interrupted_field(shared, mirror, slot, false);
    }
}

/// Read the Java `Thread.interrupted` field (interpreter round i3 wave 79,
/// lane L6; page `i74-L6-interrupt-before-start-is-not-seen-by-park-or-sleep`).
///
/// `start0` seeds the starting thread's VM flag from it: since JDK 14
/// `Thread.interrupt()` of a thread that is not alive records the interrupt
/// in this field only (`interrupt0` finds no `JavaThread`), and HotSpot's
/// `JavaThread::is_interrupted` reads the field, so the started thread's
/// first `park` returns and its first `sleep` throws. Resolved by name on
/// `java/lang/Thread` (the field the JDK reads, as the by-name writer does);
/// `false` for a receiver without the field (a synthetic `Thread`). Not a
/// hot path: once per thread start.
#[cold]
#[inline(never)]
pub(crate) fn read_thread_interrupted_field(shared: &SharedVm, thread_obj: ObjectRef) -> bool {
    let heap = &shared.mem.heap;
    let (obj, validated) = heap.load_and_forward_checked(thread_obj);
    if !validated {
        return false;
    }
    let slot = {
        let cm = shared.classes.class_manager.read();
        cm.get_loaded_class_id("java/lang/Thread")
            .and_then(|thread_class| {
                resolve_field_index_in_hierarchy(thread_class, "interrupted", &cm.class_store)
            })
    };
    match slot {
        Some(slot) if slot < heap.get_header(obj).num_slots() as usize => {
            matches!(heap.get_field(obj, slot), Value::Int(v) if v != 0)
        }
        _ => false,
    }
}

/// `Thread$FieldHolder.threadStatus` words (HotSpot's `JavaThreadStatus`, which
/// `jdk.internal.misc.VM.toThreadState` decodes). SBR round 2, lane `thread`.
///
/// The JDK's own `Thread.threadState()` / `Thread.start()` /
/// `Thread.isTerminated()` bytecode reads this field; this VM used to leave it
/// at `0` for the life of the thread and answer `getState()` from the thread
/// registry instead (`native_thread_get_state`). The VM now writes it at every
/// state change it already tracks: start (`RUNNABLE`), a blocking region's
/// begin and end (`deposit_root_snapshot` / `check_post_block_gc`, which map
/// `GcBlockState::java_state`), and termination (`TERMINATED`).
pub(crate) const THREAD_STATUS_RUNNABLE: i32 = 0x0005;
pub(crate) const THREAD_STATUS_TERMINATED: i32 = 0x0002;
pub(in crate::vm::vm_exec) const THREAD_STATUS_IN_OBJECT_WAIT: i32 = 0x0191;
pub(in crate::vm::vm_exec) const THREAD_STATUS_IN_OBJECT_WAIT_TIMED: i32 = 0x01A1;
pub(in crate::vm::vm_exec) const THREAD_STATUS_BLOCKED_ON_MONITOR_ENTER: i32 = 0x0401;

/// `GcBlockState::java_state` value of a thread that has written TERMINATED
/// into its `holder.threadStatus` and is on its way to `mark_dead`.
pub(crate) const JAVA_STATE_TERMINATING: u8 = 4;

/// The `threadStatus` word for a `GcBlockState::java_state` value
/// (`1` WAITING, `2` BLOCKED, `3` TIMED_WAITING, `4` terminating, anything
/// else RUNNABLE).
#[inline]
pub(in crate::vm::vm_exec) fn thread_status_word_for_java_state(java_state: u8) -> i32 {
    match java_state {
        JAVA_STATE_TERMINATING => THREAD_STATUS_TERMINATED,
        1 => THREAD_STATUS_IN_OBJECT_WAIT,
        2 => THREAD_STATUS_BLOCKED_ON_MONITOR_ENTER,
        3 => THREAD_STATUS_IN_OBJECT_WAIT_TIMED,
        _ => THREAD_STATUS_RUNNABLE,
    }
}

/// JMM's `JMM_THREAD_STATE_FLAG_NATIVE` bit of `ThreadJmxSnapshot::thread_status`
/// (`ThreadInfo.isInNative()`; HotSpot's `jmm.h`). Interpreter round i2 wave
/// 59, lane L6.
pub(in crate::vm::vm_exec) const JMM_THREAD_STATE_FLAG_NATIVE: i32 = 0x0040_0000;

/// At the end of a blocking region: a `BlockKind::InNative` region's
/// `jvm_thread::JAVA_STATE_IN_NATIVE` goes back to 0 (RUNNABLE), so a later
/// deposit that stores no state of its own does not report the thread in
/// native (interpreter round i2 wave 59, lane L6). One load and a branch;
/// the flag is already down, so no reader looks at the word now.
#[inline]
pub(in crate::vm::vm_exec) fn lower_in_native_state(thread: &JvmThread) {
    let state = &thread.gc_block_state.java_state;
    if state.load(std::sync::atomic::Ordering::Relaxed)
        == crate::threading::jvm_thread::JAVA_STATE_IN_NATIVE
    {
        state.store(0, std::sync::atomic::Ordering::Release);
    }
}

/// A park or an `Object.wait` publishes its kind
/// (`jvm_thread::JAVA_WAIT_KIND_PARKED` / `JAVA_WAIT_KIND_OBJECT_WAIT`) before
/// its flag rises; lowered by [`lower_wait_kind`] (interpreter round i2 wave
/// 64, lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`). One load of a field
/// fixed at construction when the switch is off.
#[inline]
pub(in crate::vm::vm_exec) fn publish_wait_kind(shared: &SharedVm, thread: &JvmThread, kind: u8) {
    if shared.threads.thread_registry.published_all_wait_kinds() {
        thread
            .gc_block_state
            .java_wait_kind
            .store(kind, std::sync::atomic::Ordering::Release);
    }
}

/// The published kind of an `Object.wait` (`monitor_wait_with_state`), at
/// the point `publish_wait_kind` publishes it: `JAVA_WAIT_KIND_OBJECT_WAIT`
/// with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`, or, for a served stand-in's wait
/// that stands for JDK code that parks (`served_parked`, decided by
/// `monitor_wait_as` under `CRATONVM_SERVED_WAIT_KINDS`),
/// `JAVA_WAIT_KIND_MONITOR_PARKED` whatever that switch says: JVMTI reads it
/// ahead of the waiting monitor (`ThreadRegistry::published_monitor_parked`).
/// Lowered with every other kind by `lower_wait_kind` once the waiting
/// monitor is taken. Interpreter round i2 wave 65, lane L1.
#[inline]
pub(in crate::vm::vm_exec) fn publish_monitor_wait_kind(shared: &SharedVm, thread: &JvmThread, served_parked: bool) {
    if served_parked {
        thread.gc_block_state.java_wait_kind.store(
            crate::threading::jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED,
            std::sync::atomic::Ordering::Release,
        );
    } else {
        publish_wait_kind(
            shared,
            thread,
            crate::threading::jvm_thread::JAVA_WAIT_KIND_OBJECT_WAIT,
        );
    }
}

/// A blocking region opens with no recorded wait kind (a plain
/// `begin_blocking_region` / `begin_timed_blocking_region`, or
/// `begin_blocking_region_as` with `BlockKind::Waiting` / `TimedWaiting`):
/// count it at its caller's source line under
/// `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` (interpreter round i2 wave 66, lane
/// L1; `docs/feature-designs/real-jdk-mode.md`).
/// `#[track_caller]`, so the location is the door's caller. One load of a
/// field fixed at construction when the census is off.
#[inline]
#[track_caller]
pub(in crate::vm::vm_exec) fn note_unrecorded_wait_region(shared: &SharedVm, door: &'static str) {
    let registry = &shared.threads.thread_registry;
    if registry.unrecorded_wait_census_on() {
        registry.note_unrecorded_wait_region(std::panic::Location::caller(), door);
    }
}

/// At the end of a blocking region, after the flag is down: a
/// `BlockKind::Sleeping` region's `GcBlockState::java_wait_kind` goes back to
/// `JAVA_WAIT_KIND_NONE`, so a later wait the thread publishes through
/// another path (a park, a timed `Object.wait`) never reads as a sleep
/// (interpreter round i2 wave 63, lane L1, `CRATONVM_PUBLISHED_WAIT_KIND`).
/// One load when nothing was published. Wave 64 (lane L1): also at the end
/// of a park (`park_with_state`) and of an `Object.wait`, after its waiting
/// monitor is taken (`monitor_wait_with_state`).
#[inline]
pub(in crate::vm::vm_exec) fn lower_wait_kind(thread: &JvmThread) {
    let kind = &thread.gc_block_state.java_wait_kind;
    if kind.load(std::sync::atomic::Ordering::Relaxed)
        != crate::threading::jvm_thread::JAVA_WAIT_KIND_NONE
    {
        kind.store(
            crate::threading::jvm_thread::JAVA_WAIT_KIND_NONE,
            std::sync::atomic::Ordering::Release,
        );
    }
}

impl NativeContextImpl<'_> {
    /// The publishing half of `NativeContext::begin_blocking_region_as_on`
    /// (interpreter round i2 wave 68, lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`;
    /// `docs/feature-designs/real-jdk-mode.md`).
    ///
    /// Called by a running mutator before the region opens. An `Object.wait`
    /// kind publishes `obj` in the registry's waiting-monitor slot (what
    /// `monitor_wait` publishes: a scanned, forwarded root, read by JMX's lock
    /// name and JVMTI's monitor answers); a park kind stores `obj` in the
    /// mirror's `parkBlocker`, which is what `LockSupport.park(blocker)` does
    /// and what the JMX `ThreadInfo` builder reports for a thread with no
    /// waiting monitor (the mirror is a root, so the field keeps `obj` alive
    /// and forwarded). Nothing is replaced: a monitor already in the slot, or a
    /// `parkBlocker` some Java frame set, stays, and then nothing is withdrawn
    /// either. Records what it did in `JvmThread::region_wait_object` for
    /// [`Self::withdraw_region_wait_object`].
    pub(in crate::vm::vm_exec) fn publish_region_wait_object(&mut self, obj: ObjectRef, kind: cratonvm_native_api::BlockKind) {
        use crate::threading::jvm_thread::{
            REGION_WAIT_OBJECT_MONITOR, REGION_WAIT_OBJECT_NONE, REGION_WAIT_OBJECT_PARK_BLOCKER,
        };
        use cratonvm_native_api::BlockKind;
        if self.thread.region_wait_object != REGION_WAIT_OBJECT_NONE
            || !self.shared.threads.thread_registry.stand_in_wait_objects()
        {
            return;
        }
        match kind {
            BlockKind::ObjectWait | BlockKind::TimedObjectWait => {
                if self
                    .shared
                    .threads
                    .thread_registry
                    .publish_region_waiting_monitor(self.thread.thread_id, obj)
                {
                    self.thread.region_wait_object = REGION_WAIT_OBJECT_MONITOR;
                }
            }
            BlockKind::Parked | BlockKind::TimedParked => {
                let Some(thread_obj) = self.thread.java_thread_obj else {
                    return;
                };
                let Some(slot) = thread_field_slot(
                    self.shared,
                    self.shared.mem.heap.class_id_of(thread_obj),
                    "parkBlocker",
                ) else {
                    return;
                };
                if !matches!(
                    self.shared.mem.heap.get_field(thread_obj, slot),
                    Value::Object(None)
                ) {
                    return;
                }
                NativeHeapAccess::set_field(&*self, thread_obj, slot, Value::Object(Some(obj)));
                self.thread.region_wait_object = REGION_WAIT_OBJECT_PARK_BLOCKER;
            }
            BlockKind::Waiting
            | BlockKind::TimedWaiting
            | BlockKind::InNative
            | BlockKind::Sleeping => {}
        }
    }

    /// Withdraw what [`Self::publish_region_wait_object`] published, at the
    /// region's end, once the thread runs again (the mirror read here is the
    /// forwarded one). One byte load when nothing was published.
    pub(in crate::vm::vm_exec) fn withdraw_region_wait_object(&mut self) {
        use crate::threading::jvm_thread::{
            REGION_WAIT_OBJECT_MONITOR, REGION_WAIT_OBJECT_NONE, REGION_WAIT_OBJECT_PARK_BLOCKER,
        };
        let published =
            std::mem::replace(&mut self.thread.region_wait_object, REGION_WAIT_OBJECT_NONE);
        if published == REGION_WAIT_OBJECT_MONITOR {
            self.shared
                .threads
                .thread_registry
                .withdraw_region_waiting_monitor(self.thread.thread_id);
        } else if published == REGION_WAIT_OBJECT_PARK_BLOCKER {
            let Some(thread_obj) = self.thread.java_thread_obj else {
                return;
            };
            if let Some(slot) = thread_field_slot(
                self.shared,
                self.shared.mem.heap.class_id_of(thread_obj),
                "parkBlocker",
            ) {
                NativeHeapAccess::set_field(&*self, thread_obj, slot, Value::Object(None));
            }
        }
    }
}



/// Write the two liveness fields of a Java `Thread` mirror: `eetop` (nonzero
/// while the thread is alive, `0` after it terminated; the JDK's
/// `Thread.alive()` is `eetop != 0`) and `holder.threadStatus`.
///
/// `eetop` is skipped when `None`, `threadStatus` when `None`. Allocates
/// nothing. A receiver without the real fields (a synthetic `Thread` stub, a
/// mirror whose `holder` is not built yet) is left alone.
pub(crate) fn set_thread_state_fields(
    shared: &SharedVm,
    thread_obj: ObjectRef,
    eetop: Option<i64>,
    status: Option<i32>,
) {
    let heap = &shared.mem.heap;
    let (obj, validated) = heap.load_and_forward_checked(thread_obj);
    if !validated {
        return;
    }
    let (eetop_slot, holder_slot) = {
        let cm = shared.classes.class_manager.read();
        let Some(thread_class) = cm.get_loaded_class_id("java/lang/Thread") else {
            return;
        };
        (
            resolve_field_index_in_hierarchy(thread_class, "eetop", &cm.class_store),
            resolve_field_index_in_hierarchy(thread_class, "holder", &cm.class_store),
        )
    };
    if let (Some(v), Some(slot)) = (eetop, eetop_slot) {
        heap.set_field(obj, slot, Value::Long(v));
    }
    if let (Some(status), Some(holder_slot)) = (status, holder_slot) {
        if let Value::Object(Some(holder)) = heap.get_field(obj, holder_slot) {
            let status_slot = {
                let cm = shared.classes.class_manager.read();
                resolve_field_index_in_hierarchy(
                    heap.class_id_of(holder),
                    "threadStatus",
                    &cm.class_store,
                )
            };
            if let Some(slot) = status_slot {
                heap.set_field(holder, slot, Value::Int(status));
            }
        }
    }
}

/// A live thread's `eetop`: nonzero, and distinct per registry thread (HotSpot
/// stores the `JavaThread*`; nothing reads more than "nonzero").
#[inline]
pub(in crate::vm::vm_exec) fn live_eetop(tid: ThreadId) -> i64 {
    0x1000 + tid.0 as i64
}

/// What `Vm::dispatch_main_uncaught_exception` did with an exception
/// that escaped `main` (gcf f4/r).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainUncaughtDispatch {
    /// No handler would have run: `ThreadGroup.uncaughtException` would print
    /// `Exception in thread "main"` and the trace, which the launcher renders
    /// itself (`vm-cli`'s uncaught report). Nothing was run.
    NoHandler,
    /// A handler ran and returned; HotSpot prints nothing more.
    Handled,
    /// The handler threw; HotSpot's `JavaThread::exit` line
    /// (`Exception: <class> thrown from the UncaughtExceptionHandler in thread
    /// "<name>"`) has been printed on stderr.
    HandlerThrew,
    /// No handler was installed and the genuine
    /// `Thread.dispatchUncaughtException` -> `ThreadGroup.uncaughtException`
    /// chain ran and returned: it printed `Exception in thread "<name>" ` and
    /// `printStackTrace(System.err)` itself, exactly as HotSpot's does (nio21
    /// wave 8 lane L4; only when the caller asked for it with `always`).
    ReportedByThreadGroup,
}

/// Would `Thread.dispatchUncaughtException` reach anything but the plain
/// `ThreadGroup.uncaughtException` print for `thread_obj`? True for a
/// per-thread handler (side table or the real field), a default handler (side
/// table or the real static), or a `ThreadGroup` SUBCLASS anywhere on the
/// group's parent chain (its override decides). Reads fields only: no
/// allocation, no Java.
pub(in crate::vm::vm_exec) fn main_thread_has_uncaught_handler(
    shared: &SharedVm,
    ctx: &dyn NativeContext,
    thread_obj: ObjectRef,
) -> bool {
    use cratonvm_native_builtins::uncaught_handlers as uh;
    if uh::get_uncaught_handler(ctx, thread_obj).is_some()
        || uh::real_thread_handler(ctx, thread_obj).is_some()
        || uh::default_uncaught_handler(ctx).is_some()
        || uh::real_default_handler(ctx).is_some()
    {
        return true;
    }
    // JDK 19+ keeps `group` on `Thread$FieldHolder`; older layouts on `Thread`.
    let mut group = match ctx.get_field_by_name(thread_obj, "holder") {
        Value::Object(Some(holder)) => ctx.get_field_by_name(holder, "group"),
        _ => ctx.get_field_by_name(thread_obj, "group"),
    };
    // Bounded: a group chain is a few levels deep (`main` -> `system`).
    for _ in 0..64 {
        let Value::Object(Some(g)) = group else {
            return false;
        };
        let cid = shared.mem.heap.class_id_of(g);
        let plain = shared
            .classes
            .class_manager
            .read()
            .get_class(cid)
            .is_some_and(|c| &*c.name == "java/lang/ThreadGroup");
        if !plain {
            return true;
        }
        group = ctx.get_field_by_name(g, "parent");
    }
    false
}

impl super::Vm {
    /// gcf f4/r (`gcf-f2r-main-thread-uncaught-exception-ignores-the-default-handler`):
    /// give an exception that escaped `main` to the main thread's uncaught
    /// handler, as HotSpot's launcher does (`LEAVE()` detaches the thread,
    /// whose `JavaThread::exit` runs `Thread.dispatchUncaughtException`).
    ///
    /// When no handler would run (`main_thread_has_uncaught_handler`) this
    /// runs nothing and answers [`MainUncaughtDispatch::NoHandler`]: the
    /// launcher then prints the trace in its own framing, which is exactly
    /// what `ThreadGroup.uncaughtException` would print. Otherwise it runs
    /// `dispatch_uncaught_exception_shared` -- the platform-thread death
    /// path's dispatch -- on the main thread. Returns the throwable's current
    /// address (the dispatch runs Java and can collect; it is pinned).
    ///
    /// `always` (nio21 wave 8 lane L4): run the dispatch even when no handler
    /// is installed, so the JDK's own `ThreadGroup.uncaughtException` prints
    /// the report -- through `System.err` (a `System.setErr` redirect is
    /// honoured), with the thread's current name, and with HotSpot's partial
    /// output when the throwable's `toString()` throws -- which is what the
    /// platform-thread death path already does for every other thread. The
    /// answer is then [`MainUncaughtDispatch::ReportedByThreadGroup`] instead
    /// of `NoHandler`.
    pub fn dispatch_main_uncaught_exception(
        &mut self,
        exc: ObjectRef,
        always: bool,
    ) -> (ObjectRef, MainUncaughtDispatch) {
        // Reporting may need a lazy Thread mirror before entering Java. Catch
        // only that local allocation refusal, never replay the user's handler.
        let pin_base = self.main_thread.native_pin_roots.len();
        self.main_thread.native_pin_roots.push(exc);
        let result = crate::runtime::native_oom::catch_alloc_oom(|| {
            let exc = self.main_thread.native_pin_roots[pin_base];
            self.dispatch_main_uncaught_exception_inner(exc, always)
        });
        let exc_now = self.main_thread.native_pin_roots[pin_base];
        self.main_thread.native_pin_roots.truncate(pin_base);
        match result {
            Ok(value) => value,
            Err(_) => {
                // JIT r23 wave 1 L11: the Java name (`JavaThread::name()`),
                // which a `Thread.setName` in `main` changes; fields only.
                let name = self
                    .main_thread_java_name()
                    .unwrap_or_else(|| self.main_thread.name.clone());
                eprintln!("\nException: java.lang.OutOfMemoryError thrown from the UncaughtExceptionHandler in thread \"{name}\"");
                (exc_now, MainUncaughtDispatch::HandlerThrew)
            }
        }
    }

    pub(in crate::vm::vm_exec) fn dispatch_main_uncaught_exception_inner(
        &mut self,
        exc: ObjectRef,
        always: bool,
    ) -> (ObjectRef, MainUncaughtDispatch) {
        let shared: &SharedVm = &self.shared;
        let thread: &mut JvmThread = &mut self.main_thread;
        let pin_base = thread.native_pin_roots.len();
        thread.native_pin_roots.push(exc);
        let (thread_obj, has_handler) = {
            let mut nctx = NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            let thread_obj = nctx.current_thread_object();
            let has = main_thread_has_uncaught_handler(shared, &nctx, thread_obj);
            (thread_obj, has)
        };
        if !has_handler && !always {
            let exc_now = thread.native_pin_roots[pin_base];
            thread.native_pin_roots.truncate(pin_base);
            return (exc_now, MainUncaughtDispatch::NoHandler);
        }
        let exc_now = thread.native_pin_roots[pin_base];
        let thread_class = shared.mem.heap.class_id_of(thread_obj);
        let result =
            dispatch_uncaught_exception_shared(shared, thread, thread_obj, exc_now, thread_class);
        let exc_now = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        let thrown = match result {
            Ok(_) if has_handler => return (exc_now, MainUncaughtDispatch::Handled),
            Ok(_) => return (exc_now, MainUncaughtDispatch::ReportedByThreadGroup),
            // No handler and the VM itself failed to run the JDK chain (not a
            // Java exception): nothing was printed, so let the launcher render
            // the report as before.
            Err(MethodCallFailed::InternalError(_)) if !has_handler => {
                return (exc_now, MainUncaughtDispatch::NoHandler);
            }
            Err(MethodCallFailed::ExceptionThrown(t)) => {
                let cid = shared.mem.heap.class_id_of(t);
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(cid)
                    .map(|c| c.name.replace('/', "."))
                    .unwrap_or_else(|| "java.lang.Throwable".to_string())
            }
            Err(other) => format!("{other:?}"),
        };
        // The thread's name as `Thread.getName()` would answer it (a program
        // may have renamed `main`); `current_thread_object` re-reads the
        // registered mirror, wherever the dispatch's collections moved it.
        let name = {
            let mut nctx = NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            let obj = nctx.current_thread_object();
            let read = match nctx.get_field_by_name(obj, "name") {
                Value::Object(Some(s)) => nctx.read_string(s),
                _ => None,
            };
            read.unwrap_or_else(|| "main".to_string())
        };
        // HotSpot 25.0.3 (`JavaThread::exit`, measured): a blank line first.
        eprintln!(
            "\nException: {thrown} thrown from the UncaughtExceptionHandler in thread \"{name}\""
        );
        (exc_now, MainUncaughtDispatch::HandlerThrew)
    }
}

/// Run `java.lang.Thread.exit()` on a thread that is terminating — the Java-side
/// teardown HotSpot's VM performs and this one never did.
///
/// `Thread.exit()`'s javadoc is the contract: *"This method is called by the VM
/// to give a Thread a chance to clean up before it actually exits."* Nothing in
/// CratonVM called it, on either death path, in any mode, so its whole body was
/// dead code here. JDK 25's body (`java.base/java/lang/Thread.java`, read from
/// `src.zip`) is:
///
/// ```text
/// private void exit() {
///     try { if (headStackableScopes != null) StackableScope.popAll(); }
///     finally { ThreadContainer c = threadContainer(); if (c != null) c.remove(this); }
///     try { if (terminatingThreadLocals() != null) TerminatingThreadLocal.threadTerminated(); }
///     finally { clearReferences(); }
/// }
/// ```
///
/// `container.remove(this)` is the load-bearing one and the reason this landed:
/// it is the ONLY decrement of `ThreadFlock.threadCount`, and that decrement is
/// what unparks an owner blocked in `awaitAll()`. Measured on the pre-change
/// binary with a thread started through the JDK's own
/// `Thread.start(ThreadContainer)`, the count stayed at 1 after the worker died
/// and a `join()` issued at that point never returned in 3/3 runs, where HotSpot
/// returned in 0-1 ms — see
/// docs/feature-designs/real-jdk-mode.md §4. That is
/// why the registration half in `native-builtins/src/shared_secrets_bridge.rs`
/// is interlocked off behind `VM_REMOVES_THREADS_FROM_CONTAINERS` until this
/// exists: adding a thread to a container that is never emptied converts
/// "`join()` waits for nothing" into "`join()` waits forever".
///
/// # Why this is called before any teardown step, not after
///
/// `exit()` runs arbitrary Java: it can allocate, it can reach a safepoint, and
/// `StackableScope.popAll()` is documented "this may block". So it must run
/// while the caller is still an ordinary live mutator. Every step that follows
/// on both death paths removes a precondition it needs:
///
/// * `tlab.retire()` — fills the TLAB's unused tail with a walkable filler.
///   Allocating after that is not unsound (`retire()` is idempotent and the
///   next carve refills), but it is pointless churn on a thread that has been
///   declared finished.
/// * `enter_inflated_or_contend(wake_obj)` — from there the caller OWNS the
///   monitor on its own `Thread` object. Running Java that blocks while holding
///   it is the three-way deadlock `Monitor::block_enter`'s doc comment warns
///   about, and `block_enter` never checks safepoints.
/// * `flush_thread_satb()` — after the drain, any reference this thread
///   overwrites is logged into a buffer whose only strong `Arc` dies with the
///   OS thread, so a marker loses gray sources it needed.
/// * `mark_dead()` — after it the collector no longer scans this thread's
///   frames or locals. Java run past that point allocates objects nothing roots:
///   a use-after-free, not a cleanup.
/// * `release_monitors_held_by_except()` — sweeps every monitor still held.
///
/// The one thing that must precede it is the uncaught-exception dispatch, and
/// not only for tidiness: `clearReferences()` nulls `uncaughtExceptionHandler`,
/// so running `exit()` first would delete the handler before it was consulted.
/// HotSpot orders it the same way — `JavaThread::exit` calls `Thread.exit()`
/// after the handler has run and before it posts JVMTI `ThreadEnd` and before
/// `ensure_join`'s notify — so the call sites place this between the handler
/// block and the JFR/JVMTI thread-end events.
///
/// # Resolution
///
/// `exit` is `private`, so it is resolved on `java/lang/Thread` itself rather
/// than on the receiver's runtime class: a `Thread` subclass (or
/// `ThreadBuilders$BoundVirtualThread`, which every "virtual" thread on this VM
/// really is) does not inherit a private method into its own dispatch surface,
/// and a same-named method on the subclass must NOT win. `invoke_special_shared_on_class`
/// is the entry point that says exactly that — invokespecial semantics, walk to
/// the declaring class, dispatch there and only there, no iface/abstract
/// retarget — and its pre-resolved form skips the loader-blind by-name lookup
/// (`java/lang/Thread` is bootstrap and unambiguous, but the ambiguity guard is
/// not free and not worth touching from a thread-death path).
///
/// # Errors are reported, never propagated
///
/// A throw out of `exit()` cannot be allowed to escape: everything after this
/// call on both paths is what marks the thread dead and notifies the monitor
/// that wakes `Thread.join()`, so an early return would trade one leak for a
/// guaranteed hang. It is equally not swallowed — a silent `let _ =` here would
/// hide, for instance, `ThreadFlock.onExit`'s `assert removed` firing under
/// `-ea`, which is the precise signature of a double-remove. So: log at WARN,
/// unconditionally, naming the thread and the error, and carry on.
///
/// Answers whether `exit()` ended structurally (returned, threw, or was not
/// run): `false` only for a VM error propagated past its frames, which keeps
/// the platform death's full monitor sweep (interpreter round i4 wave 99,
/// lane L6; `jdk_continuation::platform_death_may_skip_sweep`).
pub(in crate::vm::vm_exec) fn run_thread_exit_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    thread_obj: ObjectRef,
) -> bool {
    // zt t2/rv (`zgc-zr11rv9-z8-z9-review-residuals-FIXED-20261019.md` item 1): a
    // dying thread never reports the outcome of a ZGC native room skip it
    // still holds, and while it holds the one-slot record every other
    // thread's skip is refused. Release it here, and again after `exit()`
    // (Java code that may take one). One relaxed load when none is held;
    // nothing off ZGC (no skip is ever taken there).
    crate::runtime::interpreter::gc_and_alloc::zgc_native_room_skip_served(shared, thread);
    // Resolve `java/lang/Thread` without loading it: a thread that is
    // terminating has already run Java, so the class is loaded. Then check
    // `exit()V` is actually there before invoking, so a class library whose
    // `java/lang/Thread` is a stub without it (a `--synthetic-jdk` build; every
    // mode this binary ships supports has the real one) is a silent no-op
    // rather than a `NoSuchMethodError` warning on every single thread death.
    //
    // Routed through `MemberResolver` rather than a bare `find_method_recursive`
    // -- same idiom as `native_override.rs`'s `native_override_target_has_bytecode`.
    // Also populates the per-VM `LinkResolver` with the resolution
    // `invoke_special_shared_on_class` below will ask for anyway.
    let (thread_cid, has_exit) = {
        let cm = shared.classes.class_manager.read();
        match cm.get_loaded_class_id("java/lang/Thread") {
            Some(cid) => {
                let resolver = crate::runtime::resolve::MemberResolver::new(shared);
                let has_exit = resolver
                    .declared_method(&cm, resolver.scope(cid), "exit", "()V")
                    .is_ok();
                (Some(cid), has_exit)
            }
            None => (None, false),
        }
    };
    let Some(thread_cid) = thread_cid.filter(|_| has_exit) else {
        return true;
    };

    let result = invoke_special_shared_on_class(
        shared,
        thread,
        thread_cid,
        "java/lang/Thread",
        "exit",
        "()V",
        &[Value::Object(Some(thread_obj))],
    );
    let structural = !matches!(result, Err(MethodCallFailed::InternalError(_)));
    // zt t2/rv: and a skip `exit()` itself took (see the top of this fn).
    crate::runtime::interpreter::gc_and_alloc::zgc_native_room_skip_served(shared, thread);
    if let Err(e) = result {
        // Drain the native-return slot for the same reason the uncaught-handler
        // block a few hundred lines down drains it: a leftover there is read as
        // a substitute for a thrown exception by the next consumer, and
        // misattributes it.
        thread.native_pending_return = None;
        tracing::warn!(
            "Thread.exit() failed on terminating thread tid={} name={:?}: {:?} — its \
             Java-side cleanup (StackableScope.popAll / container.remove / \
             clearReferences) did not complete, so a ThreadContainer may still hold \
             this thread. Termination continues regardless, so Thread.join() is \
             still woken.",
            thread.thread_id.0,
            thread.name,
            e,
        );
    }
    structural
}

/// Build the just-attached JNI thread's `java.lang.Thread` now (interpreter
/// round i4 wave 85, lane L6; `jni::build_foreign_thread_object`), and put
/// it in `group` when that is a `ThreadGroup`: HotSpot's `allocate_threadObj`
/// passes the attach's group to the `Thread` constructor. The mirror is built
/// by the ordinary lazy path (`current_thread_object`, main group), then its
/// `holder.group` (and a legacy `Thread.group` field) is re-pointed; `group`
/// is pinned across the build, which allocates and runs Java.
pub(crate) fn build_attached_thread_object(
    shared: &SharedVm,
    thread: &mut JvmThread,
    group: Option<ObjectRef>,
) {
    // A VM that has not loaded `java.lang.Thread` (a bare fixture) keeps the
    // lazy mirror: building one here would load the class from an attach.
    if shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/lang/Thread")
        .is_none()
    {
        return;
    }
    let pin = thread.native_pin_roots.len();
    if let Some(g) = group {
        thread.native_pin_roots.push(g);
    }
    {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        let _ = ctx.current_thread_object();
    }
    let group = group.map(|_| thread.native_pin_roots[pin]);
    thread.native_pin_roots.truncate(pin);
    let (Some(group), Some(mirror)) = (group, thread.java_thread_obj) else {
        return;
    };
    let class_id = shared.mem.heap.class_id_of(mirror);
    let group_class = shared.mem.heap.class_id_of(group);
    let (is_group, thread_group_slot) = {
        let cm = shared.classes.class_manager.read();
        let is_group = cm
            .get_loaded_class_id("java/lang/ThreadGroup")
            .is_some_and(|tg| group_class == tg || cm.is_subclass_of(group_class, tg));
        (
            is_group,
            resolve_field_index_in_hierarchy(class_id, "group", &cm.class_store),
        )
    };
    if !is_group {
        return;
    }
    if let Some(holder_slot) = thread_field_slot(shared, class_id, "holder") {
        if let Value::Object(Some(holder)) = shared.mem.heap.get_field(mirror, holder_slot) {
            let holder_class = shared.mem.heap.class_id_of(holder);
            let slot = {
                let cm = shared.classes.class_manager.read();
                resolve_field_index_in_hierarchy(holder_class, "group", &cm.class_store)
            };
            if let Some(slot) = slot {
                shared
                    .mem
                    .heap
                    .set_field(holder, slot, Value::Object(Some(group)));
            }
        }
    }
    if let Some(slot) = thread_group_slot {
        shared
            .mem
            .heap
            .set_field(mirror, slot, Value::Object(Some(group)));
    }
}

/// The Java side of `DetachCurrentThread` for a JNI-attached thread that has
/// a `java.lang.Thread` mirror (interpreter round i4 wave 84, lane L6).
/// HotSpot's `jni_DetachCurrentThread` runs `JavaThread::exit(false,
/// jni_detach)`: `Thread.exit()`, then `ensure_join` -- under the mirror's
/// monitor, `threadStatus` TERMINATED and `eetop` 0, then `notifyAll` -- so a
/// Java thread that kept the attached thread's `Thread` sees `isAlive()`
/// false and a `join()` returns. The detach used to do neither: the mirror
/// stayed alive forever and a `join()` on it never returned.
///
/// The caller is this thread, as a counted mutator (the detach's
/// `ForeignCallGuard`), before the attachment is marked dead. A thread whose
/// mirror was never built has nothing Java can hold and is skipped. The
/// monitor entry is the ordinary GC-safe blocking one; the mirror is re-read
/// from `java_thread_obj` (remapped on every collection) after `exit()`.
///
/// Interpreter round i5 wave 107 (lane L1, page
/// `interpreter-i106-L1-jni-detach-posts-thread-death-after-join-FIXED-20261016`): split in
/// two, this (`Thread.exit()`) and [`join_attached_thread_java_side`]
/// (`ensure_join`), so the detach posts JVMTI `ThreadEnd` / JDWP
/// `ThreadDeath` -- and holds the thread while that event set suspended it --
/// between them, as `JavaThread::exit` does (`native::jni`
/// `jni_detach_current_thread`).
///
/// `uncaught` (interpreter round i5 wave 107, lane L1; page
/// `interpreter-i107-L1-jni-detach-drops-the-pending-exception-FIXED-20261016`): the JNI
/// exception the attached thread left pending. HotSpot's `JavaThread::exit`
/// takes the thread's pending exception first and runs
/// `Thread.dispatchUncaughtException` with it, before `Thread.exit()` -- so
/// the default handler prints `Exception in thread "<name>" ...` -- and on a
/// handler that throws prints `Exception: <class> thrown from the
/// UncaughtExceptionHandler in thread "<name>"`. The detach dropped it
/// silently. The dispatch is the platform thread's
/// (`dispatch_uncaught_exception_shared`).
pub(crate) fn exit_attached_thread_java_side(
    shared: &SharedVm,
    thread: &mut JvmThread,
    uncaught: Option<ObjectRef>,
) {
    let Some(mirror) = thread.java_thread_obj else {
        return;
    };
    if let Some(exc) = uncaught {
        let thread_class = shared.mem.heap.class_id_of(mirror);
        let result = dispatch_uncaught_exception_shared(shared, thread, mirror, exc, thread_class);
        if let Err(MethodCallFailed::ExceptionThrown(thrown)) = result {
            let thrown = {
                let cid = shared.mem.heap.class_id_of(thrown);
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(cid)
                    .map(|c| c.name.replace('/', "."))
                    .unwrap_or_else(|| "java.lang.Throwable".to_string())
            };
            // `Thread.getName()` as it reads now, through the mirror the
            // collections the dispatch ran keep current.
            let mirror_now = thread.java_thread_obj;
            let mut name = None;
            if let Some(obj) = mirror_now {
                let nctx = NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                name = match nctx.get_field_by_name(obj, "name") {
                    Value::Object(Some(s)) => nctx.read_string(s),
                    _ => None,
                };
            }
            let name = name.unwrap_or_else(|| thread.name.to_string());
            eprintln!(
                "\nException: {thrown} thrown from the UncaughtExceptionHandler in thread \"{name}\""
            );
        }
    }
    // Re-read: the dispatch ran Java (a GC point).
    let Some(mirror) = thread.java_thread_obj else {
        return;
    };
    run_thread_exit_shared(shared, thread, mirror);
}

/// The `ensure_join` half of the detach's Java side (see
/// [`exit_attached_thread_java_side`]): under the
/// mirror's monitor TERMINATED and `eetop` 0, then `notifyAll`. The mirror
/// is read from `java_thread_obj` (remapped on every collection).
pub(crate) fn join_attached_thread_java_side(shared: &SharedVm, thread: &mut JvmThread) {
    let Some(mirror) = thread.java_thread_obj else {
        return;
    };
    let mirror = monitor_enter_blocking(shared, thread, mirror);
    set_thread_state_fields(shared, mirror, Some(0), Some(THREAD_STATUS_TERMINATED));
    // As the platform thread's termination: the deposits of the detach's
    // last transitions must not map a stale state over TERMINATED.
    thread
        .gc_block_state
        .java_state
        .store(JAVA_STATE_TERMINATING, std::sync::atomic::Ordering::Release);
    let tid = thread.thread_id;
    if let Err(e) = shared.threads.monitors.notify_all(mirror, tid) {
        tracing::warn!("DetachCurrentThread: join notify failed for tid={}: {:?}", tid.0, e);
    }
    if let Err(e) = monitor_exit_and_retract_jmx(shared, mirror, tid) {
        tracing::warn!("DetachCurrentThread: termination monitor exit failed for tid={}: {:?}", tid.0, e);
    }
}

/// The Java side of HotSpot's launcher `DetachCurrentThread` of the MAIN
/// thread, which `JavaMain`'s `LEAVE()` performs BEFORE `DestroyJavaVM` waits
/// for the non-daemon threads (interpreter round i5 wave 101, lane L6;
/// `VmConfig::main_thread_ends_before_destroy`, page
/// `i101-L6-main-thread-stays-alive-during-destroy-java-vm`): `Thread.exit()`,
/// then under the mirror's monitor `eetop` 0, `threadStatus` TERMINATED and
/// `notifyAll`, so a non-daemon thread's `mainThread.join()` returns and
/// `mainThread.isAlive()` reads false while the launcher waits. Before this,
/// such a `join()` never returned and the process never exited.
///
/// Unlike [`join_attached_thread_java_side`] the thread's VM state is NOT
/// moved to TERMINATING and its registry entry stays alive: the launcher's
/// main `JvmThread` still waits in a blocked region and then runs the
/// shutdown sequence (HotSpot runs it on the new `DestroyJavaVM` thread).
pub(crate) fn end_main_thread_java_side(shared: &SharedVm, thread: &mut JvmThread) {
    let Some(mirror) = thread.java_thread_obj else {
        return;
    };
    run_thread_exit_shared(shared, thread, mirror);
    let Some(mirror) = thread.java_thread_obj else {
        return;
    };
    // Interpreter round i5 wave 104 (lane L1): HotSpot's `JavaThread::exit`
    // posts JVMTI `ThreadEnd` (JDWP `ThreadDeath`) after `Thread.exit()`,
    // before `ensure_join`; and from here on `main` and the `DestroyJavaVM`
    // thread this entry becomes have their own JDWP ids
    // (`debug::note_main_thread_ended`). The agent's callback may reach a
    // safepoint: the mirror is read again after it.
    #[cfg(feature = "experimental-debug")]
    {
        crate::debug::note_main_thread_ended(
            shared,
            &thread.name,
            mirror,
            thread.thread_id.0 == 0,
        );
        // Interpreter round i5 wave 105, lane L6 (page
        // `i101-L6-main-thread-stays-alive-during-destroy-java-vm`, residue):
        // a `ThreadDeath` of `main` with `SUSPEND_EVENT_THREAD` (or
        // `SUSPEND_ALL`) suspends the ended `main` (`ENDED_MAIN_VM_ID`), and
        // HotSpot's `main` stays in `JavaThread::exit` until the debugger
        // resumes it: its `ensure_join` (TERMINATED, the joiners' wake-up)
        // and the `DestroyJavaVM` attach come after. Hold here, GC-blocked,
        // until then; one load while no suspension is in force.
        //
        // Wave 105 hand-back: the gates are NOT the test. The ended `main`
        // is listed ZOMBIE by `note_main_thread_ended`, and
        // `DebugState::any_suspension_of_a_live_thread` leaves a ZOMBIE's own
        // suspension out, so `suspension_in_force()` read false and
        // `hold_native_thread_while_suspended` (which asks it first) returned
        // at once: the joiner woke while the debugger held the event set
        // (`L6W105JdiMainHeldAtThreadDeath`, `joined while main held=true`).
        // Ask the debug state itself, and wait on its change notification
        // (bounded, so a resume that publishes no change is still seen).
        let ended_main = crate::debug::ids::ENDED_MAIN_VM_ID;
        let held = |shared: &SharedVm| {
            shared
                .debug
                .debug_state
                .lock()
                .holds_off_monitor_entry(ended_main)
        };
        if thread.thread_id.0 == 0 && held(shared) {
            let mut ctx = NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            // A wait HotSpot performs inside the VM (`main` is RUNNABLE in
            // `JavaThread::exit`), not a park: no JDK wait kind.
            ctx.begin_vm_wait_region();
            {
                let gates = &shared.debug.debugger_gates;
                let mut ds = shared.debug.debug_state.lock();
                while ds.holds_off_monitor_entry(ended_main) {
                    gates.await_state_change(&mut ds, std::time::Duration::from_millis(50));
                }
            }
            ctx.end_blocking_region();
        }
    }
    let Some(mirror) = thread.java_thread_obj else {
        return;
    };
    let mirror = monitor_enter_blocking(shared, thread, mirror);
    set_thread_state_fields(shared, mirror, Some(0), Some(THREAD_STATUS_TERMINATED));
    let tid = thread.thread_id;
    if let Err(e) = shared.threads.monitors.notify_all(mirror, tid) {
        tracing::warn!("main thread end: join notify failed: {:?}", e);
    }
    if let Err(e) = monitor_exit_and_retract_jmx(shared, mirror, tid) {
        tracing::warn!("main thread end: termination monitor exit failed: {:?}", e);
    }
    // Wave 103 (lane L6): HotSpot's detach of `main` (`JavaThread::exit`,
    // `JfrThreadLocal::on_exit`) writes its `jdk.ThreadEnd`; the event
    // buffer stays (the entry keeps running as `DestroyJavaVM`).
    if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
        let ended = thread.java_thread_obj.unwrap_or(mirror);
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        cratonvm_native_builtins_jfr::jfr_vm_events::thread_ended(&mut ctx, ended);
    }
    // The Java-visible doors (served `isAlive` / `join` / `getState` /
    // `getThreadGroup`, the enumeration) read the registry, not the mirror,
    // and the blocked regions below would publish their state over
    // TERMINATED: mark the entry (it stays alive for the VM).
    shared.threads.thread_registry.mark_java_side_ended(tid);
    list_destroy_java_vm_thread(shared, thread);
}

/// HotSpot's `jni_DestroyJavaVM` attaches the waiting thread anew as
/// `DestroyJavaVM` (`AttachCurrentThread` with that name: `Thread(main
/// group, "DestroyJavaVM")`, a fresh `Thread.tid`, non-daemon, RUNNABLE), so
/// after `main` returned `Thread.getAllStackTraces()`, `ThreadMXBean` and
/// `jstack` list a `DestroyJavaVM` thread with no frames while the non-daemon
/// threads run, and the shutdown hooks run with it as `currentThread()`
/// (interpreter round i5 wave 102, lane L6; `VmConfig::destroy_java_vm_thread`,
/// page `i101-L6-main-thread-stays-alive-during-destroy-java-vm`). Here the
/// launcher's main `JvmThread` (still waiting, still the registry's id 0)
/// takes a newly built mirror under that name; the ended `main` mirror keeps
/// its `Thread.tid` index to this entry and reads terminated
/// (`ThreadRegistry::java_side_ended_for`). A failed build restores the old
/// mirror (nothing is listed, as before).
///
/// Wave 103 (lane L6) writes the attach's JFR `jdk.ThreadStart`; wave 104
/// (lane L1) posts its JVMTI and JDWP `ThreadStart`
/// (`debug::note_destroy_java_vm_attached`).
pub(in crate::vm::vm_exec) fn list_destroy_java_vm_thread(shared: &SharedVm, thread: &mut JvmThread) {
    let tid = thread.thread_id;
    let registry = &shared.threads.thread_registry;
    if !registry.java_side_ended(tid) || registry.destroy_java_vm_listed() {
        return;
    }
    let Some(old_mirror) = thread.java_thread_obj else {
        return;
    };
    // The ended `main` must keep resolving to this entry once it is no
    // longer the entry's mirror (the pointer walk would miss it and answer
    // NEW): make sure its `Thread.tid` is in the index (re-setting the same
    // mirror changes nothing else).
    if let Some(old_tid) = read_java_thread_tid(shared, old_mirror).filter(|t| *t != 0) {
        registry.set_java_thread_obj_with_tid(tid, old_mirror, old_tid);
    }
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(old_mirror);
    let old_name = std::mem::replace(&mut thread.name, "DestroyJavaVM".to_string());
    thread.java_thread_obj = None;
    let built = crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
        let mut ctx = NativeContextImpl { shared, thread };
        ctx.build_current_thread_object()
    })
    .and_then(|built| built);
    let old_mirror = thread
        .native_pin_roots
        .get(pin_base)
        .copied()
        .unwrap_or(old_mirror);
    thread.native_pin_roots.truncate(pin_base);
    let Ok(mirror) = built else {
        thread.name = old_name;
        thread.java_thread_obj = Some(old_mirror);
        registry.set_java_thread_obj(tid, old_mirror);
        return;
    };
    let mirror = thread.java_thread_obj.unwrap_or(mirror);
    // The build gave the primordial `Thread.tid` (registry id 0); the attach
    // mints one, as HotSpot's `Thread.<init>` does.
    let tid_slot = thread_field_slot(shared, shared.mem.heap.class_id_of(mirror), "tid");
    let java_tid = match tid_slot {
        Some(slot) => {
            let java_tid = cratonvm_native_builtins::mint_vm_thread_java_tid(shared.vm_identity);
            shared
                .mem
                .heap
                .set_field(mirror, slot, Value::Long(java_tid));
            u64::try_from(java_tid).unwrap_or(0)
        }
        None => 0,
    };
    registry.set_java_thread_obj_with_tid(tid, mirror, java_tid);
    registry.set_thread_name(tid, "DestroyJavaVM");
    // Wave 104 (lane L6): the dump's `elapsed=` from the attach.
    registry.restamp_registered_at(tid);
    registry.mark_destroy_java_vm_listed();
    // Interpreter round i5 wave 104 (lane L1): the attach's JVMTI and JDWP
    // `ThreadStart` (HotSpot's `attach_current_thread`), under the
    // `DestroyJavaVM` thread's own JDWP id.
    #[cfg(feature = "experimental-debug")]
    crate::debug::note_destroy_java_vm_attached(shared);
    // Wave 103 (lane L6): the attach's `jdk.ThreadStart` (HotSpot's
    // `JfrThreadLocal::on_start` for an attached thread; no parent).
    if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        cratonvm_native_builtins_jfr::jfr_vm_events::thread_started(&mut ctx, mirror, None);
    }
}

#[inline]
pub(in crate::vm::vm_exec) fn normalize_system_property_key(key: &str) -> &str {
    key.trim_matches(|c: char| c.is_ascii_control() || c == '\0')
}

impl<'a> NativeContextImpl<'a> {
    pub(in crate::vm::vm_exec) fn capture_current_stack_trace(&self) -> Vec<StackTraceEntry> {
        let cm = self.shared.classes.class_manager.read();
        let trace =
            crate::runtime::stackwalker::capture_full_trace(&cm.class_store, &self.thread.frames);
        drop(cm);
        if crate::runtime::env_cache::dbg_sttrace() {
            eprintln!(
                "STTRACE_DBG_CAP frames={} depth={}",
                self.thread.frames.len(),
                trace.len()
            );
            for (i, e) in trace.iter().enumerate() {
                eprintln!("  STTRACE_DBG_CAP[{i}] {}.{}", e.class_name, e.method_name);
            }
        }
        trace
    }

    /// H2-CID0-BLOCKED — catch the blocked-frame face of the `ClassId(0)`
    /// family at the moment it becomes observable, instead of minutes later
    /// on whatever call site happens to dereference it first.
    ///
    /// A thread parked in a native publishes its frames exactly once
    /// (`deposit_root_snapshot`) and is then invisible to every collector
    /// except through that snapshot. If the snapshot omits a live frame slot,
    /// the young sweep reclaims and ZEROES the object under it and the owner
    /// resumes reading an all-zero header. What that looks like from Java
    /// depends only on which bytecode touches it first:
    ///
    /// * `NoSuchMethodError java/lang/Object.hasNext()Z` — the `for (Future
    ///   job : jobs)` iterator in `TestMultiThread.testConcurrentUpdate`;
    /// * `CloneNotSupportedException` — `java.lang.Object` is not `Cloneable`,
    ///   so a zeroed header turns `super.clone()` into a plausible-looking
    ///   application error with no GC smell at all;
    /// * `java.lang.Object cannot be cast to X`, or a bare SIGSEGV.
    ///
    /// Only the first had a reporter, and it fired 1 run in 16. This audit
    /// asks the same question of every frame slot at every wake, so the event
    /// rate is bounded by the DEFECT rather than by which bytecode ran next.
    ///
    /// Cost: one header load per object slot (`ClassId(0)` slots are rare on a
    /// healthy run — only a genuine `new Object()` reads that way), and the
    /// free-list verdict, which takes the young and old heap locks, is reached
    /// only for those. A process-wide probe budget bounds a workload that
    /// really does park with `Object` locals.
    ///
    /// See `bug-h2-classid0-stale-address-family-FIXED.md`.
    pub(in crate::vm::vm_exec) fn audit_frames_for_reclaimed_slots(&self, site: &'static str) {
        crate::memory::reclaim_guard::audit_thread_frames(self.shared, self.thread, site);
    }

    /// Deposit a root snapshot of this thread's frames into the shared registry.
    /// Called before any blocking operation so GC can scan this thread's roots.
    ///
    /// Operand-stack-sourced roots are filtered against `heap.is_heap_addr`
    /// (alignment + arena containment) — the SAME screen `scan_local_objects`
    /// applies to locals, and deliberately not the strict `is_object_address`
    /// header probe. See `scan_frame_roots` in
    /// `runtime/interpreter/gc_and_alloc.rs` for the full rationale.
    pub(crate) fn deposit_root_snapshot(&mut self) {
        // A thread that is about to PARK cannot consult its per-thread JIT memo
        // caches, but this deposit publishes them as GC roots — so any entry
        // left in them pins its `map`/`node` (and everything those reference)
        // for the entire, unbounded blocked window. For a pooled worker that is
        // effectively forever: an idle Tomcat `http-nio-*-exec-N` kept a
        // (HashMap, Node) pair from a JSP compilation alive, and through it the
        // JDT compiler graph -> JspCompilationContext -> JasperLoader -> the
        // JSP's `Class` mirror, so `WeakReference<Class>` never cleared and
        // Tomcat's annotation cache never shrank
        // (`TestDefaultInstanceManager.testClassUnloading`, doc 26).
        //
        // Both caches are PURE MEMOS: every lookup re-validates (`modCount` +
        // key equality for the HashMap node cache, source identity for the
        // case cache) and a miss simply recomputes. Dropping them here is
        // therefore always semantically safe, and it must happen BEFORE the
        // snapshot is built so the entries are neither published as roots nor
        // left behind as stale addresses to be read after the park.
        self.thread.jit_hashmap_string_node_cache.clear();
        self.thread.string_case_cache.clear();
        self.deposit_root_snapshot_inner(true);
    }

    /// nio21 wave 17 L3: [`Self::deposit_root_snapshot`] for the blocking
    /// region's entry (`begin_blocking_region_with_state`), the one deposit
    /// whose wake (`check_post_block_gc_inner`) takes the deferred chain back.
    /// Under `CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT=1` its compiled-chain walk
    /// is deferred to the collector when it reads only frozen frames
    /// (`conservative_roots::DeferredJitChain`); everything else is
    /// `deposit_root_snapshot`'s, byte for byte. Switch off (the default):
    /// exactly `deposit_root_snapshot`.
    pub(in crate::vm::vm_exec) fn deposit_root_snapshot_for_blocking_region(&mut self) {
        self.thread.jit_hashmap_string_node_cache.clear();
        self.thread.string_case_cache.clear();
        let lazy = crate::jit::conservative_roots::lazy_blocked_jit_deposit_enabled()
            // A terminating thread's window does not end in a wake.
            && self
                .thread
                .gc_block_state
                .java_state
                .load(std::sync::atomic::Ordering::Acquire)
                != 4
            // Nor is foreign code's (an FFM downcall region, which upcalls and
            // the lazy in-native protocol can leave and re-enter without this
            // deposit's wake): those keep the eager scan.
            && !self
                .thread
                .gc_block_state
                .foreign_native
                .load(std::sync::atomic::Ordering::Acquire);
        self.deposit_root_snapshot_inner_with(true, lazy, false);
    }

    /// Finding 1(c) — snapshot refresh WITHOUT raising `in_blocked_region`.
    ///
    /// Used by the wake path (`check_post_block_gc_refs`) AFTER
    /// `GcBarrier::leave_blocked_region_flagged` has atomically cleared the
    /// flag: the thread is a counted mutator again, and transiently re-raising
    /// the flag here would re-open the excluded-while-running window (a pause
    /// whose census lands on the transient flag excludes a thread that then
    /// resumes bytecode mid-collection).
    pub(crate) fn deposit_root_snapshot_no_flag(&self) {
        self.deposit_root_snapshot_no_flag_with(false);
    }

    /// [`Self::deposit_root_snapshot_no_flag`]; `skip_chain` (nio21 wave 18
    /// L5, the lazy wake: only from `check_post_block_gc_inner`, for an
    /// unfolded window whose entry deferred its chain, under
    /// `conservative_roots::lazy_blocked_jit_wake_enabled`) leaves the
    /// compiled-chain walk out of the refresh. See the lazy-wake section in
    /// `conservative_roots` for why no reader needs it.
    pub(in crate::vm::vm_exec) fn deposit_root_snapshot_no_flag_with(&self, skip_chain: bool) {
        // SBR5 lane ncall: under the frozen-band memo, like the blocking
        // region's entry deposit (`begin_blocking_region_with_state`): the wake
        // refresh re-read the whole native stack band a second time per
        // blocking native.
        crate::jit::conservative_roots::with_frozen_band_memo(None, || {
            self.deposit_root_snapshot_inner_with(false, false, skip_chain);
        });
    }

    pub(in crate::vm::vm_exec) fn deposit_root_snapshot_inner(&self, raise_blocked_flag: bool) {
        self.deposit_root_snapshot_inner_with(raise_blocked_flag, false, false);
    }

    /// The deposit. `lazy_jit` (nio21 wave 17 L3; only ever true from
    /// [`Self::deposit_root_snapshot_for_blocking_region`], so only with
    /// `raise_blocked_flag`) defers the compiled-chain walk to the collector.
    /// `skip_chain` (nio21 wave 18 L5; only ever true from the lazy wake, so
    /// only WITHOUT `raise_blocked_flag`) leaves it out.
    pub(in crate::vm::vm_exec) fn deposit_root_snapshot_inner_with(
        &self,
        raise_blocked_flag: bool,
        lazy_jit: bool,
        skip_chain: bool,
    ) {
        // nio21 wave 17 L3: a deposit replaces the snapshot, and with it any
        // chain walk an earlier deposit deferred: cleared BEFORE anything is
        // published, so no pause pairs this deposit's snapshot with an older
        // window's chain. One uncontended lock, only while the switch is on.
        //
        // EXCEPT a deposit made with the flag already up (a nested blocking
        // region): a pause may be gathering roots right now and must keep
        // reading the outer window's record, which still describes these
        // frames exactly (no compiled code ran since). That deposit scans its
        // chain eagerly and leaves the record alone.
        let lazy_switch = crate::jit::conservative_roots::lazy_blocked_jit_deposit_enabled();
        let flag_already_up = lazy_switch
            && self
                .thread
                .gc_block_state
                .in_blocked_region
                .load(std::sync::atomic::Ordering::Acquire);
        let lazy_jit = lazy_jit && raise_blocked_flag && lazy_switch && !flag_already_up;
        if lazy_switch && !flag_already_up {
            *self.thread.gc_block_state.deferred_jit_chain.lock() = None;
        }
        // ki3: the eager twin of the record above (`deposit_walked_jit_chain`),
        // with the record's life: a deposit made with the flag down forgets
        // it, and only a flag-raising deposit that walks the chain below sets
        // it again. A nested deposit (flag already up) leaves the outer
        // window's answer alone, as it leaves the record.
        let flag_up_at_entry = self
            .thread
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire);
        if !flag_up_at_entry {
            self.thread
                .gc_block_state
                .deposit_walked_jit_chain
                .store(false, std::sync::atomic::Ordering::Release);
        }
        // Only for a window this deposit opens and its wake closes: not a
        // terminating thread's, nor foreign code's (an FFM downcall region,
        // which upcalls and the lazy in-native protocol can leave and re-enter
        // without this deposit) -- the lazy deposit's own exclusions.
        //
        // gcj j1/jn: the switch is read LAST. It is a flag-snapshot lookup
        // (`runtime_flag_default_on`, two hash probes), and a JNI native's
        // in-native deposit -- one per JNI call with the package -- has
        // `foreign_native` up already (`jni::raise_foreign_native` runs before
        // it), so it now answers `false` without the lookup. The conjuncts are
        // side-effect-free loads, so the value is unchanged on every path.
        let mark_walked_chain = raise_blocked_flag
            && !flag_up_at_entry
            && self
                .thread
                .gc_block_state
                .java_state
                .load(std::sync::atomic::Ordering::Acquire)
                != 4
            && !self
                .thread
                .gc_block_state
                .foreign_native
                .load(std::sync::atomic::Ordering::Acquire)
            && crate::jit::xt_root_scan::helper_window_covered_chain_pin_only_enabled();
        // gce e1/j: the JNI phase census's sub-phases of the package's
        // in-native deposit (`CRATONVM_DBG_JNI_PHASE=1`); `None`, and every
        // lap below a not-taken branch, otherwise.
        let census = &self.shared.natives.jni_phase_census;
        let mut clock = crate::native::jni::deposit_phase_clock(census);
        // gcf f2/j: the JNI in-native repeat (`CRATONVM_JNI_IN_NATIVE_REPEAT`,
        // on with the package): inert (`off`) in every other deposit. Read
        // BEFORE this deposit's own `root_publish_seq` bump below.
        let mut repeat =
            crate::native::jni::InNativeRepeat::begin(self.shared, self.thread, raise_blocked_flag);
        // gcf f4/j: this thread's JIT band scans below insert no movable /
        // unrewritable claims (`conservative_roots::note_movable_band_root`):
        // only this thread's own collection reads those sets, after clearing.
        let _deposit_scope = crate::native::jni::RootDepositScope::enter();
        self.thread
            .root_publish_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::memory::reclaim_guard::note_root_publish(self.shared, self.thread);
        crate::runtime::interpreter::remap_trace_push(
            self.shared,
            self.thread,
            if raise_blocked_flag {
                "deposit-block"
            } else {
                "deposit-wake"
            },
            "",
        );
        // fork6 GC_STRESS fix — flush this thread's SATB buffer before it
        // blocks. A concurrent old-gen remark drains only the GLOBAL queue;
        // a thread that logged pre-barrier entries (overwritten refs during
        // the concurrent-mark phase) and then parked would otherwise keep up
        // to ~256 entries invisible in its thread-local buffer for the whole
        // blocked duration, and the sweep would free those still-live
        // targets. Cheap no-op whenever no marking cycle is active.
        self.shared.mem.heap.flush_thread_satb();
        // Throttle-hang fix (ConcurrencyThrottleInterceptorTests, 2026-07-02):
        // this deposit is a GC-AUTHORITATIVE root publish — it is the ONLY view
        // a cross-thread STW collector has of this thread's roots for as long
        // as it stays parked. The JIT-frame portion below therefore must be a
        // FRESH scan. (Every scan is fresh since round 11 deleted the JIT scan
        // cache; the history below is why that cache could not serve here.) The
        // cache was (boundary-gen, chain-len, collection-count)-keyed, and NONE
        // of those change when compiled code inline-allocates (TLAB fast path
        // bumps no boundary generation) or when only void natives (park/unpark/
        // setCurrentBlocker) run after the last object-returning native filled
        // the cache. A JIT'd `AQS$ConditionObject.await` frame's freshly
        // allocated `ConditionNode` was exactly that: absent from the cached
        // scan, so the deposited snapshot omitted it → it was never PINNED →
        // the JIT-active non-moving young sweep's selective promotion evacuated
        // it (or reclaimed it outright) while the JIT spill slot still held the
        // young address → on wake the thread read a zeroed header
        // (`num_slots=0 ConditionNode` / `class_id=0` "Object.isReleasable"
        // NSME) → broken condition queue → every waiter parked forever (the
        // 100-thread throttle wedge). The safepoint publish path
        // (`interpreter.rs` `safepoint_check`) and `collect_roots` already
        // invalidate before their authoritative scans; this blocked-path
        // deposit was the one publish that didn't. See
        // `invalidate_scan_cache_for_gc`'s doc — it names "a parked thread's
        // pre-STW publish" as a required call site.
        crate::jit::conservative_roots::invalidate_scan_cache_for_gc();
        census.lap(crate::native::jni::JniPhase::DepPrologue, &mut clock);
        // gcf f2/j: a repeated in-native entry publishes the interpreter half
        // (class owners, frame slots and monitors; `slot_origins` below) the
        // previous one computed, which its key proves unchanged; `frames` is
        // then empty and every frame walk below scans nothing.
        let reuse_interp = repeat.reuse_interp().is_some();
        let frames: &[crate::runtime::frame::Frame] = if reuse_interp {
            &[]
        } else {
            &*self.thread.frames
        };
        // Each frame's class owner (defining user loader / non-strong hidden
        // mirror), gathered BEFORE the snapshot lock and appended after the
        // frame scan: see `roots::frame_class_owners` (gc-common w19-a).
        let class_owners = if reuse_interp {
            // gci i17/dk: `roots::FrameClassOwners` (on the stack), empty.
            crate::memory::roots::FrameClassOwners::new()
        } else {
            crate::memory::roots::frame_class_owners(&self.shared.loader_identity, self.thread)
        };
        census.lap(crate::native::jni::JniPhase::DepClassOwners, &mut clock);
        let mut snapshot = self.thread.root_snapshot.lock();
        snapshot.clear();
        snapshot.extend_from_slice(repeat.reuse_interp().unwrap_or_default());
        // Multi-thread non-moving-sweep root hardening (Fork6): see
        // `roots::conservative_locals_enabled`. A thread blocking in a native
        // (e.g. a parked FJP worker, or `main` in `f.get()`) must publish any
        // lost-tag object ref in its frame locals so selective promotion pins it
        // instead of evacuating it and zeroing the young slot under it.
        //
        // gci i18/hw (`CRATONVM_G1_VT_PRECISE_DEPOSIT`, default ON, `=0` kills):
        // NOT for a virtual thread's continuation on G1. The probes exist for a
        // lost-tag local a compiled callee left behind, and a continuation never
        // runs compiled code (`jit_bridge::assert_no_compiled_continuation`), so
        // they add only dead locals' words and tag-blind candidates -- and on G1
        // every pause MOVES, which `conservative_locals_enabled`'s own contract
        // forbids ("sound ONLY under the non-moving sweep"). A parked
        // continuation's snapshot is its one root for the whole park, so those
        // extra words were rooted (and traced) by every pause of it. See
        // `thread_registry::continuation_deposit_is_precise`.
        let conservative_locals = crate::memory::roots::conservative_locals_enabled()
            && !crate::threading::thread_registry::continuation_deposit_is_precise(
                self.shared.mem.heap.is_g1(),
                crate::threading::thread_registry::continuation_is_interpreted_only(self.thread),
                self.shared.threads.thread_registry.g1_vt_precise_deposit(),
            );
        // gcg g6/jn: what the unrecorded-young-root guard must know about this
        // snapshot (`GcBlockState::deposit_conservative`).
        self.thread
            .gc_block_state
            .deposit_conservative
            .store(conservative_locals, std::sync::atomic::Ordering::Release);
        // gch h1/jp (f1f parity): every local while a debugger can read them,
        // as `collect_roots` and `update_root_snapshot` (a config fact).
        let observes_locals = crate::runtime::jvmti::debugger_observes_locals(self.shared);
        // gch h2/fj (jp review item 1; default-ON `CRATONVM_GC_DEPOSIT_LOCALS_LIVENESS`,
        // read once per VM): under the conservative-locals rule keep the
        // liveness proof, as `update_root_snapshot` does; the tag-blind probe
        // below (`scan_locals_conservative`) still roots every lost-tag word.
        let all_live_locals = observes_locals
            || (conservative_locals && !self.shared.jit.jit_cache.deposit_locals_keep_liveness());
        for frame in frames {
            if all_live_locals {
                // While the non-moving FJP stress collector uses roots as
                // pins, scoped-out references are cheaper than a live receiver
                // reclaimed under a blocked native call. (gch h1/jp: NOT what
                // `update_root_snapshot` does -- it keeps the liveness proof
                // under the same flag and adds `scan_locals_conservative`; see
                // `gch-h1jp-dispatch-doors-and-oome-ladder-review-findings-DONE-20261012.md`.)
                frame.scan_local_objects_all_live(&mut snapshot, &self.shared.mem.heap);
            } else {
                // Same roots as `scan_local_objects`; the VM's type maps feed
                // only the `CRATONVM_DBG_VERIFY_OOP_MAPS` shadow.
                frame.scan_local_objects_mapped(
                    &mut snapshot,
                    &self.shared.mem.heap,
                    &self.shared.classes.type_maps,
                );
            }
            if conservative_locals {
                frame.scan_locals_conservative(&mut snapshot, &self.shared.mem.heap);
            }
            let before = snapshot.len();
            // Interpreter round i6 wave 122, lane L7: the frame's
            // depth-matched map row under `CRATONVM_INTERP_PRECISE_OOP_MAPS`
            // (exactly `scan_object_refs_ruled` with the switch off).
            frame.scan_stack_objects_mapped(
                &mut snapshot,
                &self.shared.mem.heap,
                crate::runtime::value_stack::MINTED_ONLY_PRIMITIVE_STACK_ROOTS,
                Some(&*self.shared.classes.type_maps),
            );
            if snapshot.len() > before {
                // gci i16/dp (i15/jk R3): filtered in place -- the same
                // entries in the same order as the `split_off(before)` and
                // re-push this was, without a `Vec` per frame
                // (`deposit_keep_heap_addrs_from`).
                deposit_keep_heap_addrs_from(&mut snapshot, before, |addr| {
                    // `is_heap_addr`, not `is_object_address` — see
                    // `scan_frame_roots`. This deposit is the collector's ONLY
                    // view of the thread once it blocks, so a root dropped here
                    // is an object reclaimed under a live frame slot.
                    self.shared.mem.heap.is_heap_addr(addr).is_some()
                });
            }
            if conservative_locals {
                frame
                    .stack
                    .scan_object_refs_conservative(&mut snapshot, &self.shared.mem.heap);
            }
            // A synchronized method's implicit monitorexit target. For
            // instance methods it duplicates local 0, but a STATIC
            // synchronized method locks the class mirror, which lives in no
            // local — without this entry the blocked-thread fixup
            // (`fold_pointer_map_into_blocked`) has no chain key for it and
            // the wake-time `monitor_on_exit` remap misses.
            if let Some(m) = frame.monitor_on_exit {
                snapshot.push(m);
            }
            // Block monitors (`Frame::held_monitors`, wave 23 lane L7): the
            // wake-time remap below needs their chain keys too.
            snapshot.extend_from_slice(frame.held_monitors.as_slice());
        }
        // A live activation keeps its class (gc-common w19-a,
        // `common-w18d-peer-interpreter-activations-do-not-keep-their-class`):
        // a thread blocked inside a method of an otherwise-unreachable
        // user-loader class keeps that loader, as the initiator's own frames do
        // in `collect_roots` §1. The blocked fold forwards these like any other
        // snapshot entry.
        snapshot.extend_from_slice(&class_owners);
        if !reuse_interp {
            // gcf f2/j: what the next repeated entry may reuse (and, under
            // `CRATONVM_DBG_JNI_REPEAT_AUDIT=1`, compared with a reused one).
            repeat.note_interp(self.shared, &snapshot[..]);
        }
        census.lap(crate::native::jni::JniPhase::DepFrames, &mut clock);
        // The JIT's stashed deopt / exceptional frames, for a thread about to
        // PARK. They live in `jit/` thread-locals, so a peer collector cannot
        // see them at all — and once this thread blocks, `GcBarrier` drops it
        // from `expected` and collects without it ever running its own scan.
        // Depositing them here is what makes that window survivable: the
        // callee-handler path can block with a frame stashed.
        // See `docs/jit/deopt-thread-local-roots.md`.
        cratonvm_jit::deopt::for_each_stashed_deopt_object(|addr| {
            if let Some(obj) = self.shared.mem.heap.is_object_address(addr as usize) {
                snapshot.push(obj);
            }
        });
        // cceres3 FIX (blocked-window exact slot tracking): record every
        // Object frame slot with the address it currently holds. GC folds
        // advance each entry's `cur` through their pointer maps; the wake
        // write-back in `check_post_block_gc_refs` stores `cur` back into
        // the exact slot. Unlike the `fixup` chain (keyed by first-move
        // addresses seeded from the FILTERED snapshot), this cannot strand a
        // slot on a missed seed, a filtered entry, or a multi-block chain.
        // Only the flag-raising deposit fills it — the wake-path refresh
        // (`deposit_root_snapshot_no_flag`) must not, since a runnable
        // thread's frames change under it.
        if raise_blocked_flag {
            let mut origins = self.thread.gc_block_state.slot_origins.lock();
            origins.clear();
            // gcf f2/j: the repeated entry's saved ones (`frames` is empty).
            origins.extend_from_slice(repeat.reuse_origins().unwrap_or_default());
            for (fi, fr) in frames.iter().enumerate() {
                // Recorded per slot, not applied as a filter: every slot is
                // still tracked and written back, but the fold's invariant
                // check needs to know which of them the collector was actually
                // obliged to keep. See `SlotOrigin::live`.
                let live_mask = fr.live_locals_mask_here();
                for li in 0..fr.locals_len() {
                    if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                        let a = o.as_ptr() as usize;
                        origins.push(crate::threading::jvm_thread::SlotOrigin {
                            frame: fi as u32,
                            idx: li as u32,
                            is_stack: false,
                            live: live_mask.is_live(li),
                            orig: a,
                            cur: a,
                        });
                    }
                }
                for si in 0..fr.stack.len() {
                    if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                        let a = o.as_ptr() as usize;
                        origins.push(crate::threading::jvm_thread::SlotOrigin {
                            frame: fi as u32,
                            idx: si as u32,
                            is_stack: true,
                            live: true,
                            orig: a,
                            cur: a,
                        });
                    }
                }
            }
            if !reuse_interp {
                repeat.note_origins(self.shared, &origins[..]);
            }
        }
        census.lap(crate::native::jni::JniPhase::DepOrigins, &mut clock);
        if crate::runtime::interpreter::remap_trace_on() {
            let snap_now: Vec<cratonvm_types::ObjectRef> = snapshot.clone();
            crate::runtime::interpreter::deposit_gap_diff(
                self.thread,
                &snap_now,
                if raise_blocked_flag { "block" } else { "wake" },
            );
        }
        // Every reference held in a `JvmThread` FIELD rather than a frame,
        // through the ONE list the initiator's scan and the safepoint deposit
        // also use (gc-common w4-b;
        // `handoff-w3b-peer-deposits-use-push-thread-field-roots.md`): native
        // invoke pins, rooted handle slots, the native old-gen allocation pool,
        // the native object in flight, the print buffer, scoped values, both
        // parked throwables, both JIT memo caches (empty on the
        // parking path -- `deposit_root_snapshot` clears them; TOMCAT-JNDIREALM-
        // JIT.3 was a case-cache entry missing from this deposit), this
        // thread's own `java.lang.Thread` mirror and any pending async
        // exception. A GC initiated by another thread marks and remaps a
        // blocked thread ONLY from this snapshot (+ the fold into its frame
        // fixup): omitting the mirror is the intermittent Tomcat
        // TestDigestAuthenticator failure (the JUnit main thread's mirror
        // orphaned while it was blocked). The wake-side remap is
        // `memory::gc::remap_thread_off_frame_refs` in `check_post_block_gc_refs`.
        crate::memory::roots::push_off_frame_thread_roots(self.thread, &mut snapshot);
        // JNI local references (INT-5): this thread's `JNI_LOCAL_FRAMES`
        // handles. A JNI native that obtained local refs and then re-entered
        // Java (parking at a safepoint) or blocked leaves them populated —
        // and a cross-thread collector marks this thread ONLY from this
        // deposited snapshot, so an object reachable solely through a parked
        // thread's JNI local was reclaimed. Thread-local storage; this
        // deposit always runs on the owning thread. The wake-side remap is
        // `update_local_refs_after_gc` in `check_post_block_gc_refs`.
        crate::native::jni::collect_local_ref_roots(&mut snapshot);
        census.lap(crate::native::jni::JniPhase::DepOffFrame, &mut clock);
        // COLLECTOR FIRST. `moving_young_enabled()` ANDs a JIT-side gate with
        // `flags().gc.moving_young` and consults the collector in neither, so
        // under G1 and ZGC this whole question is inert — and answering it is
        // not free: `refresh_moving_young_coverage_for_current_thread` ends in
        // an UNMEMOISED `native_stack_has_jit_frame` over the full band, on
        // every blocked-region entry.
        //
        // Measured on `DefaultCatalogAndSchemaTest` under the default (ZGC)
        // collector, 240 s: the probe ran 1,139,842 times reading 35.2 BILLION
        // stack words. With the moving-young term forced off
        // (`CRATONVM_NO_MOVING_YOUNG=1`) it ran 494,968 times reading 15.3
        // billion — exactly one probe per blocked deposit instead of two, and
        // 20 billion fewer words, for a decision no non-moving collector reads.
        //
        // gc-common w18-c: the coverage proof and the conservative JIT scan
        // below run with THIS VM's pause ledger bound, the way `safepoint_check`
        // binds a parking peer's deposit (`with_pause_ledger`). This deposit
        // can run inside a pause (`enter_blocked` with `pre_stw`), on a thread
        // that is not the requester and so has no binding of its own, and it
        // runs outside every pause on every blocking call. Since gc-common
        // w21-e the rows those calls write -- the moving-young verdict and its
        // reasons, the conservative-scan count, the helper-window pins
        // (`gc_quiescence::CoverageCycle`) -- are this ledger's, so a write
        // made here lands in THIS VM's rows, pause or no pause: its next
        // coverage open clears it (as it cleared the process static), and no
        // other VM's open or read sees it. The per-thread pin registry
        // `publish_pinned_jit_roots` also replaces stays process-wide, keyed by
        // thread. The read of `moving_young_coverage_incomplete` below sees
        // this VM's rows plus the process's orphan rows. Dropped right after
        // the scan's `publish_pinned_jit_roots`, below.
        let ledger_scope = cratonvm_gc::gc_quiescence::scoped_pause_ledger(
            self.shared.mem.gc_barrier.pause_ledger(),
        );
        // (The JIT scans below read this VM's type maps when the verifier
        // oracle is armed; `bind` is a cached-bool no-op otherwise.)
        let _oracle_maps = crate::jit::conservative_roots::OracleTypeMapsScope::bind(
            &self.shared.classes.type_maps,
        );
        // gcd d4/m (cross-lane, applied by d4/o): the blocked-monitor proof
        // settles on THIS thread's own chain. The VM-wide coverage rows still
        // hold the last pause's verdict outside a pause, and any helper window
        // marks them incomplete, so reading them never credited. The
        // conservative scan below still runs whenever the rows are incomplete
        // (over-retains only).
        let thread_chain_proven = self.shared.mem.heap.is_generational()
            && crate::jit::conservative_roots::moving_young_enabled()
            && crate::jit::conservative_roots::refresh_moving_young_coverage_for_current_thread();
        let moving_young_precise_only =
            thread_chain_proven && !cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete();
        census.lap(crate::native::jni::JniPhase::DepCoverage, &mut clock);
        // gcd d2/i (opt-in `CRATONVM_XT_BLOCKED_MONITOR_PROOF`): settle the arm
        // `monitor_enter_blocking_from_compiled` set -- the depth the proof
        // above covered (read AFTER it: it prunes returned entries), or 0.
        // Only a flag-raising deposit settles it, and it does so before the
        // flag below goes up, so the take-over reads it only once it is
        // final. Never armed with the flag off, so this is one load then.
        if raise_blocked_flag {
            let arm = &self.thread.gc_block_state.monitor_block_proven_jit_depth;
            if arm.load(std::sync::atomic::Ordering::Relaxed)
                == crate::threading::jvm_thread::MONITOR_BLOCK_PROOF_ARMED
            {
                let depth = if thread_chain_proven {
                    crate::jit::conservative_roots::current_thread_jit_depth()
                } else {
                    0
                };
                arm.store(depth, std::sync::atomic::Ordering::Release);
            }
        }
        // gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in): a
        // JNI in-native deposit whose band the pause scans, and which asked
        // for the proof (`conservative_roots::with_band_at_pause_proof`),
        // records the depth the proof above covered (read AFTER it, as for the
        // monitor arm), or 0. Before the flag below goes up, so a pause reads
        // only a final value (`ThreadRegistry::credit_in_native_proven_bands`).
        if raise_blocked_flag && crate::jit::conservative_roots::band_at_pause_proof_armed() {
            let depth = if thread_chain_proven {
                crate::jit::conservative_roots::current_thread_jit_depth()
            } else {
                0
            };
            self.thread
                .gc_block_state
                .in_native_proven_jit_depth
                .store(depth, std::sync::atomic::Ordering::Release);
        }

        // Cross-thread JIT-root hardening — the BLOCKING-deposit counterpart of
        // the identical fold in `interpreter::update_root_snapshot` (the
        // safepoint path). Publish the conservative roots of every active JIT
        // frame on THIS thread into the snapshot.
        //
        // A worker that parks via this path while inside a JIT-compiled frame
        // (e.g. `AbstractQueuedSynchronizer$ConditionObject.await` holding the
        // receiver `ConditionObject` and the local `ConditionNode` in JIT spill
        // slots) would otherwise deposit a snapshot that OMITS those roots: this
        // `deposit_root_snapshot` only scanned the interpreter `frames`,
        // `native_pin_roots`, and the thread mirror. A cross-thread
        // stop-the-world young collector marks a parked thread ONLY from this
        // deposited snapshot (it never runs `collect_roots` — nor the
        // thread-local `scan_active_jit_frames` — FOR a non-current thread), so
        // the JIT-held objects were unreachable and the non-moving young sweep
        // reclaimed them while still live → the all-zero-header
        // `ConditionNode`/`ConditionObject` flood during Tomcat
        // `LifecycleBase.stop()` (executor/poller/utility threads parked on a
        // `Condition`), then the `implicit monitorexit … does not own the
        // monitor` cascade and NPEs. `deposit_root_snapshot` ALWAYS runs on the
        // thread it is snapshotting (the parking thread's own native context),
        // so this thread-local scan captures exactly this worker's live JIT
        // spill region — the SAFE single-thread-in-JIT case documented in
        // `conservative_roots::scan_active_jit_frames`. It is a no-op when no
        // JIT frame is on this thread's stack (socket read / sleep / join /
        // ReferenceQueue.remove), false positives are filtered by
        // `is_object_address`, and they can only over-retain (the young sweep
        // runs non-moving while any thread is in JIT, so nothing is relocated).
        // MEASUREMENT LEVER (`CRATONVM_GC_NOFLAG_DEPOSIT_SKIP_JIT_SCAN=1`,
        // default OFF): the no-flag deposit is a republish by a thread that is
        // still RUNNING, and this scan's stated obligation is to a thread that
        // has PARKED — see `env_cache::noflag_deposit_skips_jit_scan` for the
        // measurement, the argument, and the two holes in the argument that
        // keep it off by default.
        let skip_jit_scan =
            !raise_blocked_flag && crate::runtime::env_cache::noflag_deposit_skips_jit_scan();
        if !moving_young_precise_only && !skip_jit_scan {
            let jit_scan_start = snapshot.len();
            crate::memory::native_roots::rootprof::note_scan_caller(2); // blocked-deposit
                                                                        // r12w1 g1store: the pins owed by band words the G1 root screen rejects.
            let reject_capture =
                crate::jit::conservative_roots::g1_band_reject_pins::Capture::arm_if_wanted(
                    &self.shared.mem.heap,
                    false,
                );
            if skip_chain && !raise_blocked_flag {
                // nio21 wave 18 L5: the lazy wake's refresh -- everything but
                // the chain walk, which no reader of a counted mutator's
                // snapshot needs (the lazy-wake section in `conservative_roots`).
                crate::jit::conservative_roots::scan_active_jit_frames_skipping_chain(
                    &self.shared.mem.heap,
                    &mut snapshot,
                );
            } else if lazy_jit {
                // nio21 wave 17 L3: everything but the chain walk runs here;
                // the walk, when it reads only frozen compiled frames, is
                // published for the collector (`DeferredJitChain`) before the
                // flag below goes up, and the pins below then cover only what
                // was scanned here -- the collector pins the rest per pause.
                match crate::jit::conservative_roots::scan_active_jit_frames_deferring_chain(
                    &self.shared.mem.heap,
                    &mut snapshot,
                ) {
                    crate::jit::conservative_roots::ChainCover::Deferred(deferred) => {
                        *self.thread.gc_block_state.deferred_jit_chain.lock() = Some(deferred);
                    }
                    crate::jit::conservative_roots::ChainCover::Walked => {
                        if mark_walked_chain {
                            self.thread
                                .gc_block_state
                                .deposit_walked_jit_chain
                                .store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                    crate::jit::conservative_roots::ChainCover::NotWalked => {}
                }
            } else if mark_walked_chain {
                // ki3: the eager walk of a window this deposit opens (a park,
                // a monitor wait, a join): recorded for the helper-window pass
                // (`helper_window_covered_chain_pin_only_enabled`).
                if crate::jit::conservative_roots::scan_active_jit_frames_walking_chain(
                    &self.shared.mem.heap,
                    &mut snapshot,
                ) {
                    self.thread
                        .gc_block_state
                        .deposit_walked_jit_chain
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            } else {
                crate::jit::conservative_roots::scan_active_jit_frames(
                    &self.shared.mem.heap,
                    &mut snapshot,
                );
            }
            census.lap(crate::native::jni::JniPhase::DepJitScan, &mut clock);
            // JIT round 21 lane refs, REFS21-6, second gate: an armed park's
            // flag-raising deposit has scanned (or deferred to the collector)
            // this thread's compiled chain. Before the flag goes up below.
            if raise_blocked_flag
                && self
                    .thread
                    .gc_block_state
                    .park_credit_armed
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                self.thread
                    .gc_block_state
                    .park_credit_deposited
                    .store(true, std::sync::atomic::Ordering::Release);
                if crate::runtime::env_cache::dbg_referent_holder() {
                    eprintln!(
                        "[xt-park-credit] gate=deposited thread={} lazy={lazy_jit} deferred={}",
                        self.thread.thread_id.0,
                        self.thread
                            .gc_block_state
                            .deferred_jit_chain
                            .lock()
                            .is_some(),
                    );
                }
            }
            let reject_pins = reject_capture.map(|c| c.finish()).unwrap_or_default();
            // G1 pin-in-place, cross-thread half (same as the safepoint path
            // in `update_root_snapshot`): the note above — "they can only
            // over-retain (the young sweep runs non-moving while any thread
            // is in JIT, so nothing is relocated)" — is GENERATIONAL-only.
            // G1 always evacuates, so this blocked thread's conservative JIT
            // roots must also PIN their regions out of the collection set;
            // the spill slots holding them cannot be rewritten. Replace
            // semantics per thread; entry dropped at thread exit.
            // 2026-09-06: NOT `is_g1()` any more, and the note above is why —
            // "the young sweep runs non-moving while any thread is in JIT" has
            // been false for the generational collector since the cross-thread
            // JIT coverage handshake landed (2026-08-23), and ZGC has been
            // compacting since 2026-08-13. Both consume this registry; both
            // were seeing only the INITIATOR's pins.
            // `CRATONVM_GC_G1_ONLY_JIT_PINS=1` restores the old gate — see
            // `runtime::interpreter::gc_and_alloc::g1_only_jit_pins`.
            if !crate::runtime::interpreter::gc_and_alloc::g1_only_jit_pins()
                || self.shared.mem.heap.is_g1()
            {
                // gci i16/dp (i15/jk R3): on the stack up to 32 addresses
                // (`gc_quiescence`'s republish-compare bound), not a heap
                // `Vec` per deposit; the same addresses in the same order
                // (the reject pins, when any, after the scan's).
                let mut addrs: smallvec::SmallVec<[usize; 32]> = snapshot[jit_scan_start..]
                    .iter()
                    .map(|r| r.as_ptr() as usize)
                    .collect();
                if !reject_pins.is_empty() {
                    let mut extra: Vec<usize> = Vec::new();
                    crate::jit::conservative_roots::g1_band_reject_pins::extend_published(
                        &reject_pins,
                        &mut extra,
                    );
                    addrs.extend(extra);
                }
                cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&addrs);
            }
        } else if !crate::runtime::interpreter::gc_and_alloc::g1_only_jit_pins()
            || self.shared.mem.heap.is_g1()
        {
            cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&[]);
        }
        crate::native::jni::publish_raw_local_pins();
        drop(ledger_scope);
        // gce e2/j: with the conservative scan (lapped above) the rest is the
        // pin publication; without it, this is the whole (skipped) scan.
        census.lap(crate::native::jni::JniPhase::DepJitPins, &mut clock);
        // Shadow-stack precise roots (mirrors the same fold in
        // `update_root_snapshot`): under `CRATONVM_SHADOW_STACK` the moving
        // collector may run with threads in JIT, so a cross-thread STW cycle
        // must see a parked worker's shadow-held oops or they are reclaimed.
        // No-op when the gate is off or the shadow stack is empty.
        if crate::jit::conservative_roots::shadow_stack_enabled() {
            self.thread.shadow_stack.for_each_value(|v| {
                if let Some(obj_ref) = self.shared.mem.heap.is_object_address(v) {
                    snapshot.push(obj_ref);
                }
            });
            // ki3: a deposit that skipped the chain scan because this
            // thread's whole chain is PROVEN precise (the moving-young
            // coverage proof: every live compiled frame's oops are published
            // to the shadow stack, folded into this snapshot just above) has
            // covered the chain too -- by the proof every moving young cycle
            // already relies on (`deposit_walked_jit_chain`).
            if mark_walked_chain && moving_young_precise_only {
                self.thread
                    .gc_block_state
                    .deposit_walked_jit_chain
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }

        // DIAGNOSTIC (CRATONVM_DBG_BLOCKGC) — the deposit-side precondition of
        // the blocked-frame stale-receiver family, checked WITHOUT needing the
        // rare collection to land in the window.
        //
        // This snapshot is the ONLY view a cross-thread collector has of this
        // thread while it is blocked. So a frame slot that decodes as
        // `Value::Object(Some(_))`, is LIVE at this frame's pc, and is not in
        // the snapshot, is an object the collector will not know to keep — and
        // the owner will read it again on wake. `slot_origins` (recorded just
        // above) cannot rescue it either: that tracker advances `cur` through
        // each cycle's pointer map, and an object nothing rooted is never IN a
        // pointer map.
        //
        // What can differ: this walk decodes a slot with `to_value()`, while
        // the two publishing scans additionally consult the parallel kind
        // marks (`Frame::local_kinds`, `ValueStack::kinds`) and skip a slot
        // marked `long`/`double`, so the moving collector cannot relocate a
        // pointer-shaped primitive. A slot whose kind mark and whose value tag
        // DISAGREE therefore falls between the two, and is published by
        // neither. Non-zero output here names that slot.
        if blockgc_dbg() && raise_blocked_flag {
            let published: rustc_hash::FxHashSet<usize> =
                snapshot.iter().map(|r| r.as_ptr() as usize).collect();
            let heap = &self.shared.mem.heap;
            let mut gaps = 0usize;
            for (fi, fr) in self.thread.frames.iter().enumerate() {
                let live_mask = fr.live_locals_mask_here();
                let mut report = |what: &str, idx: usize, a: usize| {
                    if published.contains(&a) || heap.is_heap_addr(a).is_none() {
                        return;
                    }
                    gaps += 1;
                    if gaps <= 8 {
                        eprintln!(
                            "[blockgc] UNPUBLISHED-LIVE-SLOT tid={} frame#{fi} {}.{} pc={} \
                             {what}[{idx}] 0x{a:x} — a live object frame slot the blocking \
                             deposit did not publish as a root",
                            self.thread.thread_id.0,
                            fr.class_name(),
                            fr.method_name(),
                            fr.pc,
                        );
                    }
                };
                for li in 0..fr.locals_len() {
                    if !live_mask.is_live(li) {
                        continue;
                    }
                    if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                        report("local", li, o.as_ptr() as usize);
                    }
                }
                for si in 0..fr.stack.len() {
                    if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                        report("stack", si, o.as_ptr() as usize);
                    }
                }
            }
            if gaps > 0 {
                eprintln!(
                    "[blockgc] deposit tid={} frames={} snapshot={} UNPUBLISHED-LIVE-SLOTS={}",
                    self.thread.thread_id.0,
                    self.thread.frames.len(),
                    snapshot.len(),
                    gaps,
                );
            }
        }
        drop(snapshot);
        census.lap(crate::native::jni::JniPhase::DepShadow, &mut clock);
        // Publish a line-less frame trace alongside the root snapshot so another
        // thread can read where THIS thread is PARKED (cross-thread
        // `Thread.getStackTrace()` / `dumpThreads()`). For a blocked thread it
        // reflects the blocking call site, which is the whole point of it.
        //
        // ONLY on the flag-raising deposit. This is a DIAGNOSTIC, not a GC root
        // — nothing in `collect_all_root_snapshots` reads it — and for a thread
        // that is still RUNNING (`deposit_root_snapshot_no_flag`: the wake path
        // and `refresh_root_snapshot`) it answers a question nobody asked: the
        // thread is not parked anywhere, and its trace is stale again before the
        // deposit returns. A running thread's dump has a live producer anyway
        // (`stw_publish_frame_traces`, which republishes at the pause).
        //
        // It is not cheap, despite "capture is lock-free": it allocates a
        // `Vec<StackTraceEntry>` with four `Arc` clones PER JAVA FRAME, every
        // time. On the synthetic-stream drain loops — which refresh the deposit
        // once per element (`stream_pull_internal`) — that is O(depth)
        // allocations per stream element. Profiling a `StackWalker.walk` at
        // depth 120 put `capture_frames_no_lines` at 3.9% and the matching
        // `drop_glue::<Vec<StackTraceEntry>>` at 3.3%, i.e. ~7% of the run spent
        // building and freeing a trace of a thread that was never parked.
        // gcf f2/j: a repeated in-native entry whose compiled activations are
        // the ones the standing trace was built from leaves that trace and its
        // JMX attribution as published (`InNativeRepeat::trace_still_published`).
        // JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`): nor
        // when a frame-trace pause already published this thread's trace for
        // the native call it blocks in (a park whose running phase that pause
        // ended): those are its frames now, listed with the native's stand-in
        // rows, and a reader that read that trace needs that attribution.
        if raise_blocked_flag
            && !repeat.trace_still_published(self.shared, self.thread)
            && !self.thread.gc_block_state.native_excuse.pause_published_trace()
        {
            // With the compiled activations (wave 38, lane L7): a thread
            // parked under compiled callers published none of them.
            let jit_frames = repeat.take_walked_frames_for_capture();
            let trace = crate::runtime::stackwalker::capture_published_trace_with_jit(
                self.shared,
                &self.thread.frames,
                &self.thread.reflective_calls,
                &self.thread.reflective_frames_named,
                &self.thread.served_thread_run,
                jit_frames,
                crate::runtime::interpreter::jit_bridge::live_osr_continuation_artifact,
            );
            let trace_len = trace.entries.len();
            *self.thread.frame_trace.lock() = trace.entries;
            // W6-F fix 3/4: the trace is a blocked deposit now, not the live
            // one a pause's record describes.
            self.shared
                .threads
                .thread_registry
                .note_trace_deposit(self.thread.thread_id);
            census.lap(crate::native::jni::JniPhase::TraceCapture, &mut clock);
            // Which of those frames locked each held monitor, for another
            // thread's `ThreadInfo.getLockedMonitors()` (wave 24, lane L7).
            self.shared
                .threads
                .thread_registry
                .publish_jmx_frame_monitors(
                    self.thread.thread_id,
                    &self.thread.frames,
                    &self.thread.jmx_frame_monitors_published,
                    trace.frame_positions.as_deref().map(|p| (p, trace_len)),
                    &trace.compiled_holds,
                );
            census.lap(crate::native::jni::JniPhase::TraceJmx, &mut clock);
        }
        if raise_blocked_flag {
            // The per-class half of the stale-frame census, before the flag
            // below goes up (interpreter round i1 wave 24, lane L3).
            crate::runtime::interpreter::obsolete_frames::publish_blocked_frame_classes(
                self.thread,
            );
        }
        census.lap(crate::native::jni::JniPhase::TraceCensus, &mut clock);
        // DIAGNOSTIC-ONLY (cceres3): catch a frame slot that is ALREADY stale
        // at block entry — the deposited snapshot then can never chain it
        // (`fold_pointer_map_into_blocked` seeds only from snapshot
        // addresses), so the wake-time fixup misses it forever. Needs both
        // CRATONVM_DBG_BLOCKGC and the CRATONVM_DBG_STALE_OBJREF ring.
        if blockgc_dbg() {
            let mut stale_n = 0usize;
            for (fi, fr) in self.thread.frames.iter().enumerate() {
                for li in 0..fr.locals_len() {
                    if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = self.shared.mem.heap.debug_forwarded_target(a) {
                            eprintln!(
                                "[blockgc] DEPOSIT-STALE tid={} frame#{fi} {}.{} pc={} local[{li}] 0x{a:x}->0x{new:x}",
                                self.thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            );
                            stale_n += 1;
                        }
                    }
                }
                for si in 0..fr.stack.len() {
                    if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = self.shared.mem.heap.debug_forwarded_target(a) {
                            eprintln!(
                                "[blockgc] DEPOSIT-STALE tid={} frame#{fi} {}.{} pc={} stack[{si}] 0x{a:x}->0x{new:x}",
                                self.thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            );
                            stale_n += 1;
                        }
                    }
                }
            }
            let pending = self.thread.gc_block_state.fixup.lock().len();
            if stale_n > 0 || pending > 0 {
                eprintln!(
                    "[blockgc] deposit tid={} raise={} frames={} fixup_pending={} stale={}",
                    self.thread.thread_id.0,
                    raise_blocked_flag,
                    self.thread.frames.len(),
                    pending,
                    stale_n,
                );
                if pending > 0 {
                    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                        eprintln!(
                            "[blockgc] deposit-with-pending tid={} caller:\n{}",
                            self.thread.thread_id.0,
                            std::backtrace::Backtrace::force_capture(),
                        );
                    }
                }
            }
        }
        // H2-CID0-BLOCKED: a slot that is ALREADY zeroed here was reclaimed
        // before this block, so the blocked window is not where it was lost —
        // the distinction the wake-side audit cannot make on its own.
        if raise_blocked_flag {
            self.audit_frames_for_reclaimed_slots("blocked frame slot (block entry)");
        }
        // gcf f2/j: record what this deposit published, under the key it holds
        // for -- BEFORE the flag goes up: from then on a pause may complete
        // and fold into the thread, and a key read after it would vouch for a
        // half computed before it.
        repeat.finish(self.shared, self.thread);
        // Mark the blocked region AFTER the snapshot is complete: from this
        // point on, every GC initiator maintains this thread's roots via
        // `fold_pointer_map_into_blocked` (snapshot remap + frame-fixup
        // composition) until `check_post_block_gc` clears the flag on wake.
        // (Skipped for the wake path's refresh — see
        // `deposit_root_snapshot_no_flag`.)
        if raise_blocked_flag {
            // SBR round 2 (lane `thread`): the state this block reports
            // (`java_state` was stored by the caller before the deposit) goes
            // into `holder.threadStatus`, where the JDK's own `threadState()`
            // reads it. Before the flag goes up: once it is visible nothing
            // on this thread may write to the heap (a census that excluded
            // us does not wait), and this is still a scanned mutator.
            self.publish_thread_status(thread_status_word_for_java_state(
                self.thread
                    .gc_block_state
                    .java_state
                    .load(std::sync::atomic::Ordering::Acquire),
            ));
            self.thread
                .gc_block_state
                .in_blocked_region
                .store(true, std::sync::atomic::Ordering::Release);
        }
        census.stop(crate::native::jni::JniPhase::DepTail, clock);
    }

    /// Re-sync this thread's GC state after waking from a blocking region
    /// (wait/park/join/ReferenceQueue-remove).
    ///
    /// Every *moving* GC that completed while this thread was blocked folded
    /// its pointer map into `gc_block_state.fixup` (composed across multiple
    /// missed collections, keyed by the addresses our frames still hold) and
    /// kept our `root_snapshot` remapped — see
    /// `ThreadRegistry::fold_pointer_map_into_blocked`. Here we apply that
    /// composed fixup to the frames and thread-local refs, refresh the
    /// snapshot, and leave the blocked region.
    ///
    /// Ordering/race notes (the barrier's `expected` accounting is what makes
    /// this sound):
    /// - The caller's `BlockedGuard` has already dropped (or
    ///   `mark_blocked_region_leave` already ran), so this thread counts as
    ///   an expected mutator: a NEW stop-the-world can start but cannot
    ///   COMPLETE until we arrive — so after the arrive-loop below drains, no
    ///   fold can race the fixup application.
    /// - The map returned by `arrive_and_wait` is deliberately DISCARDED:
    ///   `in_blocked_region` is still set while we wait, so that same GC
    ///   folded its map into our fixup; applying both would double-apply.
    /// - The flag is cleared LAST, after the snapshot refresh: a GC that
    ///   starts in between waits for us (expected), sees a fresh snapshot,
    ///   and folds nothing new.
    pub(crate) fn check_post_block_gc(&mut self) {
        self.check_post_block_gc_refs(&mut []);
    }

    /// Interpreter round i2 wave 68, lane L6
    /// (`CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT`;
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// [`Self::check_post_block_gc`] for a caller that promises the thread
    /// runs no bytecode and touches none of its frames before it is next
    /// GC-blocked or arrives at a safepoint (the debugger park's region left
    /// for heap work, `debug::ParkGcRegion::leave_for_heap_work`). With
    /// `unfolded_since` -- what the region's entry read -- and when the drain
    /// finds no fold since the last wake, an empty fixup, the barrier's
    /// generation, the heap's collection count and the redefinition count
    /// all as the entry read them, and no deferred frame write waiting, the
    /// wake skips the snapshot refresh (`deposit_root_snapshot_no_flag`) and
    /// the frame-slot reclaim audit: the snapshot the entry deposited still
    /// names every root of the unmoved frames at its current address.
    /// Everything else of the wake runs as always. Answers whether it kept
    /// the snapshot; `None` is exactly `check_post_block_gc`.
    #[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
    #[inline(never)] // gcj j1pn raw-write item 2: the wake floor (see `check_post_block_gc_inner`)
    pub(crate) fn check_post_block_gc_keeping(
        &mut self,
        unfolded_since: Option<UnfoldedSince>,
    ) -> bool {
        let wake_floor = crate::jit::conservative_roots::current_stack_pointer();
        self.check_post_block_gc_inner(&mut [], unfolded_since, false, None, wake_floor)
    }

    /// `check_post_block_gc` + re-sync of native-local raw `Value` refs
    /// (`extra_refs`) against the same accumulated fixup — used by native
    /// poll loops that captured their args before blocking (see
    /// `NativeContext::end_blocking_region_refs`).
    #[inline(never)] // gcj j1pn raw-write item 2: the wake floor (see `check_post_block_gc_inner`)
    pub(in crate::vm::vm_exec) fn check_post_block_gc_refs(&mut self, extra_refs: &mut [Value]) {
        let wake_floor = crate::jit::conservative_roots::current_stack_pointer();
        let _kept = self.check_post_block_gc_inner(extra_refs, None, false, None, wake_floor);
    }

    /// Interpreter round i4 wave 99, lane L1
    /// (`CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT`, default on): the leave of a
    /// `begin_blocking_region*` region (`end_blocking_region`,
    /// `end_blocking_region_refs`). [`Self::check_post_block_gc_refs`], except
    /// that it keeps the snapshot the region's entry deposited -- no frame
    /// audit, no rebuild -- when the key the entry recorded
    /// (`GcBlockState::blocking_leave_key`) still describes the thread: see
    /// `blocking_leave_key` for what the key covers and why that is sound.
    /// Answers whether it kept the snapshot.
    #[inline(never)] // gcj j1pn raw-write item 2: the wake floor (see `check_post_block_gc_inner`)
    pub(in crate::vm::vm_exec) fn check_post_block_gc_quiet_refs(&mut self, extra_refs: &mut [Value]) -> bool {
        let wake_floor = crate::jit::conservative_roots::current_stack_pointer();
        self.check_post_block_gc_inner(extra_refs, None, true, None, wake_floor)
    }

    /// Wave 99 (lane L1): the quiet-leave pause terms of a region about to be
    /// entered with `java_state`, read BEFORE its deposit (so a pause that
    /// completes during or after the deposit makes the leave rebuild), or
    /// `None` for a region whose leave must always rebuild: the switch off,
    /// foreign code (an FFM downcall region: C may upcall and run the lazy
    /// in-native protocol inside it), a terminating thread (no wake).
    pub(in crate::vm::vm_exec) fn blocking_leave_terms(&self, java_state: u8) -> Option<BlockingLeavePauseTerms> {
        (self
            .shared
            .threads
            .thread_registry
            .blocking_leave_keeps_snapshot()
            && java_state != 4
            && !self
                .thread
                .gc_block_state
                .foreign_native
                .load(std::sync::atomic::Ordering::Acquire))
        .then(|| blocking_leave_pause_terms(self.shared, &*self.thread))
    }

    /// Wave 99 (lane L1): record the region's quiet-leave key right after the
    /// flag-raising deposit it describes (`None` terms record `None`). Always
    /// written, so no older region's key survives an entry.
    pub(in crate::vm::vm_exec) fn record_blocking_leave_key(&mut self, terms: Option<BlockingLeavePauseTerms>) {
        // Wave 100 (lane L1, `CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS`):
        // the same key in a recycled list.
        let reuse = self
            .shared
            .threads
            .thread_registry
            .blocking_leave_key_reuses_buffers();
        let key = terms.map(|terms| {
            if reuse {
                blocking_leave_key_reusing(terms, &*self.thread)
            } else {
                blocking_leave_key(terms, &*self.thread)
            }
        });
        *self.thread.gc_block_state.blocking_leave_key.lock() = key;
    }

    /// Interpreter round i5 wave 100, lane L1: the flag-raising
    /// [`Self::deposit_root_snapshot`] of a blocking door OUTSIDE this module
    /// (the class-initialization wait in
    /// `vm_util::ensure_class_initialized_shared`), with the quiet leave's key
    /// recorded around it exactly as the doors here do (terms before the
    /// deposit, the key right after it). Pair with
    /// [`Self::check_post_block_gc_quiet`].
    pub(crate) fn deposit_root_snapshot_keyed(&mut self) {
        let java_state = self
            .thread
            .gc_block_state
            .java_state
            .load(std::sync::atomic::Ordering::Acquire);
        let leave_terms = self.blocking_leave_terms(java_state);
        self.deposit_root_snapshot();
        self.record_blocking_leave_key(leave_terms);
    }

    /// Interpreter round i5 wave 100, lane L1: the quiet leave
    /// ([`Self::check_post_block_gc_quiet_refs`] with no native-local refs)
    /// for a door that deposited through [`Self::deposit_root_snapshot_keyed`].
    /// Answers whether it kept the snapshot.
    pub(crate) fn check_post_block_gc_quiet(&mut self) -> bool {
        self.check_post_block_gc_quiet_refs(&mut [])
    }

    /// gci i48 lane jl48 (`gci-i10al-...` item 4, `ja11_w_tag`): the full
    /// wake of a JNI native call leaving native after a pause completed or
    /// folded into it (`native::jni::native_call_leave_native`). Exactly
    /// [`Self::check_post_block_gc`], except that it keeps the snapshot the
    /// window's pauses folded -- no reclaimed-slot audit, no refresh deposit
    /// -- when [`in_native_wake_keeps_snapshot`] admits it, and, with
    /// `keep_origins` (a JNIEnv function entered through
    /// `ForeignJniEntry::enter_vm_only`), leaves `slot_origins` rebased onto
    /// the slots' current addresses for the incremental re-entry. Answers
    /// whether it kept the snapshot.
    ///
    /// Why the folded snapshot is the refresh's: the thread was in native
    /// for the whole window, so its frames are the ones its last full deposit
    /// scanned (the incremental re-entries only appended JNI locals). Every
    /// relocating pause of the window folded its map into the snapshot and
    /// into the fixup chain together (`fold_one_blocked_chain`), and this
    /// wake applied the same moves to the frames, the off-frame fields, the
    /// deopt stash and the JNI local frames; so the snapshot names the same
    /// objects, at the addresses those now hold, that a rebuild would read.
    /// The refresh is refused when that argument does not hold: compiled
    /// frames below the native (their conservative words, band and coverage
    /// proof are the deposit's to redo), a deferred chain or a deferred frame
    /// write, a class redefinition the frames are not converted for, and
    /// `CRATONVM_DBG_BLOCKGC` (which keeps the rebuild's diagnostics).
    /// `CRATONVM_JNI_IN_NATIVE_KEEP_SNAPSHOT=0` (read once per VM) refuses it
    /// always: exactly `check_post_block_gc`.
    #[inline(never)] // gcj j1pn raw-write item 2: the wake floor (see `check_post_block_gc_inner`)
    pub(crate) fn check_post_block_gc_in_native(&mut self, keep_origins: bool) -> bool {
        let wake_floor = crate::jit::conservative_roots::current_stack_pointer();
        self.check_post_block_gc_inner(&mut [], None, false, Some(keep_origins), wake_floor)
    }

    /// The body of [`Self::check_post_block_gc_refs`],
    /// [`Self::check_post_block_gc_keeping`] (wave 68),
    /// [`Self::check_post_block_gc_quiet_refs`] (wave 99) and
    /// [`Self::check_post_block_gc_in_native`] (gci i48 jl48, `in_native`
    /// `Some(keep_origins)`): answers whether it kept the snapshot (never
    /// with `unfolded_since` `None`, `quiet_leave` false and `in_native`
    /// `None`).
    ///
    /// `wake_floor` (gcj j1pn raw-write item 2): an address in the CALLING
    /// wrapper's frame, taken before it called this, so every frame of this
    /// wake -- this one included -- lies below it (`apply_native_slot_fixups`).
    /// Both halves of that are load-bearing, hence the `#[inline(never)]` on
    /// this body AND on every wrapper: inlined into a wrapper, this body's
    /// locals could be laid out above the probe; and a wrapper inlined into
    /// ITS caller -- the frame that blocked, whose captured words must still
    /// be written -- would put the probe inside that frame and refuse its
    /// words below the probe. A wrapper's own frame is new at the wake.
    #[inline(never)]
    pub(in crate::vm::vm_exec) fn check_post_block_gc_inner(
        &mut self,
        extra_refs: &mut [Value],
        unfolded_since: Option<UnfoldedSince>,
        quiet_leave: bool,
        in_native: Option<bool>,
        wake_floor: usize,
    ) -> bool {
        use crate::memory::gc::update_value_ref;
        // gcj j1pn raw-write item 7: tag this wake's raw write-back audit lines.
        let _audit_vm =
            cratonvm_gc::root_write_audit::VmTagScope::enter(self.shared.vm_identity as u64);

        // Finding 1(a/c) — atomically drain every in-flight pause AND clear
        // `in_blocked_region` under one barrier-lock hold. Ordering is the
        // load-bearing part:
        //  * Pauses active while our flag is up EXCLUDED us (identity census)
        //    — we wait them out WITHOUT filling anyone's quota slot (the old
        //    participating `arrive_and_wait` drain here inflated `arrived`
        //    and released `wait_for_all` while a counted mutator still ran).
        //    Their pointer maps reach us via `fold_pointer_map_into_blocked`
        //    (flag still up ⇒ every such fold covers us, and each fold
        //    completes before its pause's generation advances).
        //  * The flag-clear lands under the same lock hold that confirmed no
        //    pause is active, so no pause can start-and-complete between the
        //    drain and the clear (the old plain `store(false)` left exactly
        //    that window: an excluded-but-running thread mutating the heap
        //    mid-collection — finding 1's corruption family).
        //  * Only AFTER the clear do we take + apply the fixup below: any
        //    newer pause counts us in `expected` and cannot complete (or
        //    fold) until our next safepoint arrival, so the application can
        //    never race a fold or an evacuation.
        self.shared.mem.gc_barrier.leave_blocked_region_flagged(
            self.thread.thread_id,
            &self.thread.gc_block_state.in_blocked_region,
        );
        // nio21 wave 17 L3: the flag is down, so no pause reads this thread's
        // deferred chain any more (`resolve_deferred_blocked_jit_chains` asks
        // only while it is up, and a pause that saw it up held this leave).
        // Take it with the window. A deposit that deferred its chain left the
        // snapshot WITHOUT the chain's roots, so the wave-68 keep below must
        // not keep that snapshot for a running thread.
        // ki3: and its eager twin, with the window.
        self.thread
            .gc_block_state
            .deposit_walked_jit_chain
            .store(false, std::sync::atomic::Ordering::Release);
        let had_deferred_chain = crate::jit::conservative_roots::lazy_blocked_jit_deposit_enabled()
            && self
                .thread
                .gc_block_state
                .deferred_jit_chain
                .lock()
                .take()
                .is_some();

        // Apply the composed fixup accumulated for every GC we slept through.
        let fixup = {
            let mut f = self.thread.gc_block_state.fixup.lock();
            std::mem::take(&mut *f)
        };
        // gcg g2/k, retired 2026-11-02 (`common-w2g-proposal-blocked-threads-replay-pause-maps`):
        // how many relocating folds composed this chain. The flag is down, so
        // no fold targets this thread any more until its next deposit.
        let folds_now = self
            .thread
            .gc_block_state
            .blocked_folds
            .load(std::sync::atomic::Ordering::Acquire);
        let folds_k = folds_now.saturating_sub(
            self.thread
                .gc_block_state
                .folds_at_last_wake
                .swap(folds_now, std::sync::atomic::Ordering::Relaxed),
        );
        // Wave 68 (lane L6): may this wake keep the snapshot the region's
        // entry deposited? Decided with the flag down: from here no pause
        // completes without this thread's arrival, so nothing read below can
        // change before the caller's promise ends. A deferred frame write
        // (one VM-wide flag, so any thread's) changes the frames: refresh.
        #[cfg(feature = "experimental-debug")]
        let frame_writes_waiting = self.shared.debug.debugger_gates.frame_writes_pending();
        #[cfg(not(feature = "experimental-debug"))]
        let frame_writes_waiting = false;
        // Interpreter round i4 wave 99, lane L1
        // (`CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT`): the entry's key, taken
        // by EVERY wake (whichever door), so it never outlives its window.
        let leave_key = self
            .thread
            .gc_block_state
            .blocking_leave_key
            .lock()
            .take();
        // The quiet leave of a `begin_blocking_region*` region: decided here,
        // with the flag down (no pause can complete without this thread from
        // now on, so nothing read below changes before the thread's next
        // publish), from the key its entry recorded. Every term is one of the
        // invalidating events of `blocking_leave_key`'s argument: a deferred
        // frame write, a fold or a non-empty fixup, a deferred chain the
        // rebuild itself would rescan (only without the lazy wake), a class
        // redefinition the thread has not converted its frames for, and the
        // key (pauses, collections, folds, redefinitions, other deposits,
        // frames, off-frame roots). `CRATONVM_DBG_BLOCKGC` keeps the rebuild
        // and its diagnostics.
        // Interpreter round i5 wave 102, lane L1: `CRATONVM_DBG_PB` names the
        // refusing term of a quiet leave (`[PB-QUIET]`), so a process-wait
        // wake gap that stays at the rebuild's cost can be attributed.
        let pb_trace = quiet_leave && cratonvm_types::flags().io.dbg_pb;
        let had_leave_key = leave_key.is_some();
        let quiet_keep = quiet_leave
            && !frame_writes_waiting
            && folds_k == 0
            && fixup.is_empty()
            && (!had_deferred_chain
                || crate::jit::conservative_roots::lazy_blocked_jit_wake_enabled())
            && !blockgc_dbg()
            && self.thread.redefinitions_seen == cratonvm_classloading::class_redefinition_count()
            && leave_key.is_some_and(|entry| {
                let terms = blocking_leave_pause_terms(self.shared, &*self.thread);
                // Wave 100 (lane L1): the same exact comparison, streamed
                // (`CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS`).
                if self
                    .shared
                    .threads
                    .thread_registry
                    .blocking_leave_key_reuses_buffers()
                {
                    blocking_leave_key_matches(entry, terms, &*self.thread)
                } else {
                    blocking_leave_key(terms, &*self.thread) == entry
                }
            });
        if pb_trace {
            let refused_by = if quiet_keep {
                "none"
            } else if frame_writes_waiting {
                "frame-writes-pending"
            } else if folds_k != 0 || !fixup.is_empty() {
                "folded-or-fixup"
            } else if had_deferred_chain
                && !crate::jit::conservative_roots::lazy_blocked_jit_wake_enabled()
            {
                "deferred-chain"
            } else if blockgc_dbg() {
                "dbg-blockgc"
            } else if self.thread.redefinitions_seen
                != cratonvm_classloading::class_redefinition_count()
            {
                "redefinition"
            } else if !had_leave_key {
                "no-entry-key"
            } else {
                "key-differs"
            };
            eprintln!(
                "[PB-QUIET] tid={} kept={} refused_by={} t_ns={}",
                self.thread.thread_id.0,
                quiet_keep,
                refused_by,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
        }
        let park_keep = !frame_writes_waiting
            && !had_deferred_chain
            && unfolded_since.is_some_and(|since| {
                folds_k == 0
                    && fixup.is_empty()
                    && self
                        .shared
                        .mem
                        .gc_barrier
                        .gc_generation
                        .load(std::sync::atomic::Ordering::Acquire)
                        == since.gc_generation
                    && self.shared.mem.heap.collection_count() == since.collections
                    && cratonvm_classloading::class_redefinition_count() == since.redefinitions
            });
        // gci i48 (lane jl48): the in-native leave keeps the folded snapshot
        // (`check_post_block_gc_in_native`). Decided with the flag down, as
        // the keeps above: no pause completes without this thread from here.
        // `CRATONVM_JNI_IN_NATIVE_KEEP_SNAPSHOT=0` (read once per VM) kills
        // it: every wake refreshes, and no call re-enters after a kept wake.
        let in_native_keep = in_native.is_some()
            && crate::native::jni::in_native_keep_snapshot_of(self.shared)
            && in_native_wake_keeps_snapshot(
                frame_writes_waiting,
                had_deferred_chain,
                blockgc_dbg(),
                self.thread.redefinitions_seen
                    == cratonvm_classloading::class_redefinition_count(),
                crate::jit::conservative_roots::current_thread_jit_depth(),
            );
        let keep_snapshot = park_keep || quiet_keep || in_native_keep;
        if crate::runtime::interpreter::remap_trace_on() {
            crate::runtime::interpreter::remap_trace_push(
                self.shared,
                self.thread,
                "wake",
                &format!("fixup={}", fixup.len()),
            );
        }
        if !fixup.is_empty() {
            // BUG-03 trace (gated): record that the blocked-wake remap ran for main.
            if self.thread.thread_id.0 == 0
                && false
            {
                let jto = self
                    .thread
                    .java_thread_obj
                    .map(|o| o.as_ptr() as usize)
                    .unwrap_or(0);
                eprintln!(
                    "[BUG03-fm] e{} path=blocked-wake(check_post_block) tid0 jto=0x{:x} jto_in_fixup={} fixup.len={}",
                    self.shared.mem.heap.collection_count(), jto, jto != 0 && fixup.contains_key(&jto), fixup.len()
                );
            }
            if blockgc_dbg() {
                eprintln!(
                    "[blockgc] wake tid={} applying {} composed fixups ({} frames) k={}",
                    self.thread.thread_id.0,
                    fixup.len(),
                    self.thread.frames.len(),
                    folds_k
                );
            }
            // THE JIT HALF OF THIS WAKE (2026-09-08), and until now it ran
            // only in the leaked-region FALLBACK. Everything below this line
            // rewrites interpreter frames and thread-local `ObjectRef`s; a
            // thread that blocked in a native with COMPILED frames below it
            // has its live oops in JIT frame slots, register images and the
            // shadow stack instead, and nothing on this path touched them.
            //
            // `apply_pointer_map_to_thread` -- the STW-resume path -- has
            // carried exactly these three calls for the thread that parked at
            // the barrier, and its comment already describes this defect for
            // that population: "this stranded a non-initiator's JIT-frame oops
            // at their old addresses after a relocation -- a use-after-free".
            // A blocked peer is the same bug one path over, and it is the
            // population a moving young cycle relocates under whenever the
            // cross-thread coverage handshake credits it.
            // Attribution: this thread applied a relocation map through the
            // ORDINARY BLOCKED-REGION WAKE.
            cratonvm_gc::gc_quiescence::note_pointer_map_applied(2);
            apply_blocked_wake_jit_remap(self.shared, self.thread, &fixup);
            for frame in &mut self.thread.frames {
                frame.update_frame_refs_mapped(
                    &fixup,
                    &self.shared.mem.heap,
                    &self.shared.classes.type_maps,
                );
                // A blocked `synchronized` method must release the RELOCATED
                // monitor object on frame-pop, not the stale address.
                if let Some(ref mut obj_ref) = frame.monitor_on_exit {
                    let old_addr = obj_ref.as_ptr() as usize;
                    if let Some(&new_addr) = fixup.get(&old_addr) {
                        *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                    }
                }
                // Its block monitors too (`Frame::held_monitors`, wave 23
                // lane L7), one lookup per entry.
                frame.held_monitors.remap_with(|a| fixup.get(&a).copied());
            }
            // The remap half for this thread's stashed deopt / exceptional
            // frames, against the same composed fixup the frames above use.
            // Deposited on the way into the blocking region; rewritten here on
            // the way out, because the collection that moved them ran while
            // this thread was parked and could not run its own remap.
            cratonvm_jit::deopt::remap_stashed_deopt_objects(|addr| {
                fixup.get(&(addr as usize)).map(|&to| to as u64)
            });
            // Every off-frame per-thread reference, through the ONE list the
            // four per-thread remap paths share
            // (`memory::gc::remap_thread_off_frame_refs`, gc-common w2-g
            // 2026-09-23): printed values, native pins, handle slots, the
            // native allocation pool, the in-flight native return, the JIT
            // HashMap node cache, the ASCII case cache, scoped-value keys and
            // values, and the three single slots.
            //
            // This wake used to carry its own copy of that list, and it missed
            // the two parked throwables (`jit_pending_exception`,
            // `uncaught_exception_pending`) the blocked deposit publishes. The
            // launcher parks `uncaught_exception_pending` and then sleeps HERE
            // (the shutdown-hook join), so a hook thread's moving collection
            // relocated the throwable and this wake left the field at the
            // vacated address -- the `Exception in thread "main"
            // java/lang/Object` render.
            crate::memory::gc::remap_thread_off_frame_refs(self.thread, &fixup);
            for val in extra_refs.iter_mut() {
                update_value_ref(val, &fixup);
            }
            // JNI local references (INT-2, blocked-wake half): rewrite THIS
            // thread's `JNI_LOCAL_FRAMES` handles through the composed fixup —
            // a JNI native that blocked mid-call (monitor, join, park) and
            // slept through moving collections must not resume with dangling
            // local jobjects. The storage is thread-local, so this wake path
            // is the only place that can reach this thread's handles.
            crate::native::jni::update_local_refs_after_gc(&fixup);
        }
        // The native-stack half of the same wake: see `apply_native_slot_fixups`.
        // AFTER the JIT remap above, never before (gc-common w2-g, 2026-09-23):
        // its `still reads orig` guard skips a word the JIT remap already
        // rewrote, whereas the JIT remap, run second, would look an
        // already-written `cur` up in the composed chain again and -- under a
        // sliding compactor's `A -> B, B -> C` map -- move it onto the wrong
        // object.
        let _native_applied = apply_native_slot_fixups(self.thread, wake_floor);

        // See `JvmThread::last_heal_collection`.
        self.thread.last_heal_collection = self.shared.mem.heap.collection_count();
        // cceres3 FIX: exact per-slot write-back for the blocked window.
        // Runs after the chain application above — any slot the chain already
        // healed reads back != orig and is skipped; any slot the chain MISSED
        // (seed gap / filtered snapshot / multi-block chain break) still
        // holds `orig` and gets the tracked current address. Kind-strict on
        // the operand stack (collision longs untouched); locals only rewrite
        // when the slot still decodes to exactly `orig` as an object.
        {
            let origins = {
                let mut o = self.thread.gc_block_state.slot_origins.lock();
                std::mem::take(&mut *o)
            };
            if !origins.is_empty() {
                let dbg = blockgc_dbg();
                for so in &origins {
                    if so.cur == so.orig {
                        continue;
                    }
                    let fi = so.frame as usize;
                    let Some(fr) = self.thread.frames.get_mut(fi) else {
                        continue;
                    };
                    if so.is_stack {
                        if fr.stack.rewrite_object_at(so.idx as usize, so.orig, so.cur) && dbg {
                            eprintln!(
                                "[blockgc] writeback healed tid={} frame#{fi} stack[{}] 0x{:x}->0x{:x}",
                                self.thread.thread_id.0, so.idx, so.orig, so.cur,
                            );
                        }
                    } else if let Value::Object(Some(o)) = fr.get_local(so.idx as u16) {
                        if o.as_ptr() as usize == so.orig {
                            // SAFETY: `cur` is the object's current address,
                            // advanced through the GC pointer maps by the
                            // initiator folds.
                            fr.set_local(
                                so.idx as u16,
                                Value::Object(Some(unsafe {
                                    ObjectRef::from_raw(so.cur as *mut u8)
                                })),
                            );
                            if dbg {
                                eprintln!(
                                    "[blockgc] writeback healed tid={} frame#{fi} local[{}] 0x{:x}->0x{:x}",
                                    self.thread.thread_id.0, so.idx, so.orig, so.cur,
                                );
                            }
                        }
                    }
                }
            }
            // gci i48 (lane jl48): a kept in-native wake whose call may go
            // back into native incrementally (`keep_origins`) leaves the same
            // slots tracked from where they are now -- what the full
            // re-entry's deposit would record of these unchanged frames. A
            // slot that does not read `cur` is skipped by the next write-back.
            if in_native_keep && in_native == Some(true) {
                let mut tracked = self.thread.gc_block_state.slot_origins.lock();
                rebase_slot_origins(&origins, &mut tracked);
            }
        }
        // JDWP (interpreter round i1 wave 41, lane L1): a `StackFrame.SetValues`
        // a debugger sent while this thread was blocked, applied now that the
        // moves it slept through are in its frames, and before the root
        // snapshot below is refreshed from them. One load unless one waits.
        #[cfg(feature = "experimental-debug")]
        crate::debug::inspect::apply_deferred_frame_writes_if_any(self.shared, self.thread);
        if folds_k > 0 || !fixup.is_empty() {
            invalidate_root_cache_after_wake_remap(self.thread);
        }
        // H2-CID0-BLOCKED: every relocation this thread slept through has now
        // been applied, so any frame slot still reading `ClassId(0)` is either
        // a genuine `new Object()` or an object that was RECLAIMED while this
        // thread was parked and could not defend it. `report_reclaimed_receiver`
        // separates the two by free-list membership.
        //
        // Wave 68 (lane L6): a kept snapshot's frames were neither moved nor
        // run since the entry's deposit audited them: no audit, no refresh.
        // (A deferred frame write is recorded only through a blocked thread's
        // inspection window, which the park region never opens; the
        // VM-wide pending check above refuses the keep anyway.)
        // Wave 99 (lane L1): the same for the quiet leave of a blocking
        // region, whose key proves it (`blocking_leave_key`).
        if !keep_snapshot {
            self.audit_frames_for_reclaimed_slots("blocked frame slot (wake)");
            // Refresh (don't clear) the snapshot: we are runnable again but may
            // not reach a safepoint before the next GC scans roots; an empty
            // snapshot would hide every object reachable only from our frames.
            // Finding 1(c): the no-flag variant — `in_blocked_region` was already
            // cleared atomically above; transiently re-raising it here would
            // re-open the excluded-while-running census window.
            //
            // nio21 wave 18 L5 (`CRATONVM_GC_LAZY_BLOCKED_JIT_WAKE`, default on
            // under the opt-in lazy deposit): a window whose entry deferred its
            // chain and that no collection folded into refreshes WITHOUT the
            // compiled-chain walk (page
            // `r21-blocking-wake-refresh-rescans-the-compiled-chain-FIXED-20261009.md`).
            let lazy_wake = had_deferred_chain
                && folds_k == 0
                && fixup.is_empty()
                && crate::jit::conservative_roots::lazy_blocked_jit_wake_enabled();
            self.deposit_root_snapshot_no_flag_with(lazy_wake);
        }
        // DIAGNOSTIC-ONLY (cceres3): verify no frame slot is left stale after
        // the wake-time fixup application — catches both "chain key missing"
        // (was_key=false) and "frame held an intermediate address" desyncs at
        // the exact wake where they surface.
        // Predicate: the FIXUP CHAIN's keys, not a forwarding word. Same
        // correction as the arrival-site ARRIVE-STALE verifier — the forwarding
        // word does not exist on the default collector (ZGC's slide leaves
        // none), so this reported zero whatever the truth was. The chain's keys
        // are exactly the pre-move addresses this thread slept through, and the
        // write-back above has just run, so a frame slot still holding one is a
        // slot the write-back did not reach.
        if blockgc_dbg() {
            for (fi, fr) in self.thread.frames.iter().enumerate() {
                for li in 0..fr.locals_len() {
                    if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = fixup.get(&a).copied().filter(|&new| new != a) {
                            eprintln!(
                                "[blockgc] WAKE-STALE tid={} frame#{fi} {}.{} pc={} local[{li}] 0x{a:x}->0x{new:x} was_key={} was_val={} fixup_len={}",
                                self.thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                                fixup.contains_key(&a),
                                fixup.values().any(|&v| v == a),
                                fixup.len(),
                            );
                        }
                    }
                }
                for si in 0..fr.stack.len() {
                    if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = fixup.get(&a).copied().filter(|&new| new != a) {
                            eprintln!(
                                "[blockgc] WAKE-STALE tid={} frame#{fi} {}.{} pc={} stack[{si}] 0x{a:x}->0x{new:x} was_key={} was_val={} fixup_len={}",
                                self.thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                                fixup.contains_key(&a),
                                fixup.values().any(|&v| v == a),
                                fixup.len(),
                            );
                        }
                    }
                }
            }
        }
        // JEP 109 obsolete methods: a class redefined while this thread was
        // blocked leaves the frames it was blocked under running a replaced
        // body; they move onto their translated copies before any of them
        // resolves another constant (interpreter round i1 wave 19, lane L3).
        // One load and a compare unless a class was redefined.
        crate::runtime::interpreter::obsolete_frames::convert_obsolete_frames_if_redefined(
            self.shared,
            self.thread,
        );
        // SBR round 2 (lane `thread`): awake again. Every relocation this
        // thread slept through is applied (the mirror in `java_thread_obj` is
        // current), and this is a counted mutator again, so `holder.threadStatus`
        // goes back to RUNNABLE, the write `deposit_root_snapshot` made
        // WAITING / BLOCKED / TIMED_WAITING at the start of the block.
        self.publish_thread_status(THREAD_STATUS_RUNNABLE);
        // Interpreter round i2 wave 56, lane L6: the frames are current; a
        // native frames window open under this region may be read again.
        #[cfg(feature = "experimental-debug")]
        crate::runtime::interpreter::note_native_frames_current(self.shared, self.thread);
        keep_snapshot
    }

    /// T19.K1вЂ” Read the daemon flag from a Java `Thread` object.
    ///
    /// Returns `Some(true|false)` if the Thread carries a resolvable
    /// daemon attribute, or `None` if the layout is unrecognised
    /// (synthetic-mode `Thread` without a daemon field, or a class
    /// without a `holder` chain). The caller defaults to non-daemon
    /// when this returns `None` вЂ” which is the JLS rule for any
    /// thread constructed from `main` (parent thread is non-daemon,
    /// child inherits, unchanged unless `Thread.setDaemon(true)`
    /// was called before `start()`).
    ///
    /// Layout walk (real-JDK mode):
    ///   1. Look up the `Thread.holder` field slot via the class
    ///      hierarchy.
    ///   2. Read `holder` вЂ” if null, the Thread isn't fully
    ///      constructed; treat as non-daemon (`Some(false)`).
    ///   3. Look up the `daemon` field on the holder's class.
    ///   4. Read it as `Value::Int` (boolean is encoded as 0/1).
    ///
    /// Synthetic-mode `Thread` doesn't have a `holder` field, so
    /// step 1 returns `None` and we fall through to `None` here.
    pub(crate) fn read_thread_daemon_flag(&self, thread_obj: ObjectRef) -> Option<bool> {
        let header = self.shared.mem.heap.get_header(thread_obj);
        let cm = self.shared.classes.class_manager.read();
        // Step 1: locate `Thread.holder` slot. Round 15 wave 4 (lane
        // threadvm): the one `java.lang.Thread` declares
        // (`thread_field_owner`). By name, a subclass's own `holder` field
        // answered: its object's `daemon` (or none) decided whether the VM
        // waits for this thread at exit.
        let holder_slot = resolve_field_index_in_hierarchy(
            thread_field_owner(self.shared, &cm.class_store, header.class_id),
            "holder",
            &cm.class_store,
        )?;
        // Step 2: read it.
        let holder_obj = match self.shared.mem.heap.get_field(thread_obj, holder_slot) {
            Value::Object(Some(o)) => o,
            // Thread allocated but holder not yet wired up вЂ” treat as
            // non-daemon. The JDK bytecode would NPE here on
            // `Thread.isDaemon()`; we just degrade to the default
            // rather than crash the VM-shutdown path.
            _ => return Some(false),
        };
        // Step 3: locate `FieldHolder.daemon` slot. Real JDK 25 names it
        // exactly `daemon`; we walk the class hierarchy in case a
        // future JDK moves it to a parent.
        let holder_header = self.shared.mem.heap.get_header(holder_obj);
        let daemon_slot =
            resolve_field_index_in_hierarchy(holder_header.class_id, "daemon", &cm.class_store)?;
        // Step 4: read the boolean.
        match self.shared.mem.heap.get_field(holder_obj, daemon_slot) {
            Value::Int(0) => Some(false),
            Value::Int(_) => Some(true),
            // Anything else (Object/Long/etc.) is layout corruption.
            // Don't trust it вЂ” return `None` so the caller falls
            // back to the safe default (non-daemon).
            _ => None,
        }
    }

    pub(crate) fn read_thread_task_object(&self, thread_obj: ObjectRef) -> Option<ObjectRef> {
        let header = self.shared.mem.heap.get_header(thread_obj);
        // Round 15 wave 4 (lane threadvm): `target` / `holder` are the fields
        // `java.lang.Thread` declares (`thread_field_owner`), not a
        // subclass's own of that name. j27 wave 3 (lane BACKLOG, TV2-5): read
        // through the per-VM thread-layout memo (`thread_field_slot`, the same
        // answer without the class-manager lock once warm), as every other
        // `Thread.start`-time reader already is. No `class_manager` guard may
        // be held here: `thread_field_slot` takes its own.
        let target_slot = thread_field_slot(self.shared, header.class_id, "target");
        let holder_slot = thread_field_slot(self.shared, header.class_id, "holder");
        if let Some(target_slot) = target_slot {
            if let Value::Object(Some(task)) =
                self.shared.mem.heap.get_field(thread_obj, target_slot)
            {
                return Some(task);
            }
        }
        if let Some(holder_slot) = holder_slot {
            if let Value::Object(Some(holder_obj)) =
                self.shared.mem.heap.get_field(thread_obj, holder_slot)
            {
                if let Some(task) = self.read_thread_holder_task(holder_obj) {
                    return Some(task);
                }
            }
        }
        // `synthetic Thread.target`. Round 15 wave 5 (lane threadvm2;
        // `r15w4-threadvm-review-findings` F6): only on a layout where neither
        // named field resolves; on the real layout slot 3 is another field.
        if target_slot.is_none() && holder_slot.is_none() && header.num_slots() > 3 {
            if let Value::Object(Some(task)) = self.shared.mem.heap.get_field(thread_obj, 3) {
                return Some(task);
            }
        }
        None
    }

    /// `FieldHolder.task` of a thread's `holder` object (JDK 19+ layout), or
    /// `None` when the holder declares no `task` or it is null.
    pub(in crate::vm::vm_exec) fn read_thread_holder_task(&self, holder_obj: ObjectRef) -> Option<ObjectRef> {
        let holder_class = self.shared.mem.heap.class_id_of(holder_obj);
        let task_slot = {
            let cm = self.shared.classes.class_manager.read();
            resolve_field_index_in_hierarchy(holder_class, "task", &cm.class_store)
        }?;
        match self.shared.mem.heap.get_field(holder_obj, task_slot) {
            Value::Object(Some(task)) => Some(task),
            _ => None,
        }
    }

    /// The `java.util.concurrent.ThreadPoolExecutor$Worker` a pool worker
    /// thread runs, or `None` for any other thread.
    pub(crate) fn thread_pool_executor_worker_task(
        &self,
        thread_obj: ObjectRef,
    ) -> Option<ObjectRef> {
        let task = self.read_thread_task_object(thread_obj)?;
        let task_cid = self.shared.mem.heap.class_id_of(task);
        self.shared
            .classes
            .class_manager
            .read()
            .class_store
            .get(task_cid)
            .is_some_and(|c| &*c.name == "java/util/concurrent/ThreadPoolExecutor$Worker")
            .then_some(task)
    }

    /// Round 15 wave 5 (lane threadvm2): set `Thread.holder.daemon` of a
    /// not-yet-running thread (the VM's own async pool worker, see
    /// `thread_start`). No-op on a layout without a holder.
    /// JVMTI `RunAgentThread` (interpreter round i2 wave 63, lane L1;
    /// `jvmti::native_env::agent_functions`): before the thread starts, make
    /// `thread_obj` a daemon of `priority` (`Thread.holder`'s `daemon` and
    /// `priority`, as HotSpot's `JavaThread::start_internal_daemon` sets
    /// them) and, for a `Thread` that ran before, not started
    /// (`threadStatus` 0), so `thread_start` starts it afresh.
    #[cfg(feature = "experimental-debug")]
    pub(crate) fn prepare_agent_thread(&self, thread_obj: ObjectRef, priority: i32) {
        self.set_thread_holder_daemon_flag(thread_obj);
        let class_id = self.shared.mem.heap.class_id_of(thread_obj);
        let Some(holder_slot) = thread_field_slot(self.shared, class_id, "holder") else {
            return;
        };
        let Value::Object(Some(holder)) = self.shared.mem.heap.get_field(thread_obj, holder_slot)
        else {
            return;
        };
        let holder_class = self.shared.mem.heap.class_id_of(holder);
        let (priority_slot, status_slot) = {
            let cm = self.shared.classes.class_manager.read();
            (
                resolve_field_index_in_hierarchy(holder_class, "priority", &cm.class_store),
                resolve_field_index_in_hierarchy(holder_class, "threadStatus", &cm.class_store),
            )
        };
        if let Some(slot) = priority_slot {
            self.shared
                .mem
                .heap
                .set_field(holder, slot, Value::Int(priority));
        }
        if let Some(slot) = status_slot {
            self.shared.mem.heap.set_field(holder, slot, Value::Int(0));
        }
    }

    pub(in crate::vm::vm_exec) fn set_thread_holder_daemon_flag(&self, thread_obj: ObjectRef) {
        let class_id = self.shared.mem.heap.class_id_of(thread_obj);
        let Some(holder_slot) = thread_field_slot(self.shared, class_id, "holder") else {
            return;
        };
        let Value::Object(Some(holder)) = self.shared.mem.heap.get_field(thread_obj, holder_slot)
        else {
            return;
        };
        let holder_class = self.shared.mem.heap.class_id_of(holder);
        let daemon_slot = {
            let cm = self.shared.classes.class_manager.read();
            resolve_field_index_in_hierarchy(holder_class, "daemon", &cm.class_store)
        };
        if let Some(daemon_slot) = daemon_slot {
            self.shared
                .mem
                .heap
                .set_field(holder, daemon_slot, Value::Int(1));
        }
    }

    /// One object allocation for the lazy thread-mirror plumbing
    /// (`current_thread_object`, [`Self::build_thread_field_holder`]).
    ///
    /// Always report refusal on its existing Result channel, without GC or
    /// spending G1's emergency reserve. The constructor owns rooting and error
    /// conversion; a direct host teardown call is fallible too.
    pub(in crate::vm::vm_exec) fn thread_plumbing_alloc(
        &self,
        class_id: ClassId,
        num_fields: usize,
    ) -> Result<ObjectRef, crate::runtime::native_oom::NativeAllocOom> {
        self.shared
            .mem
            .heap
            .try_alloc_object_full(class_id, num_fields)
            .ok_or(crate::runtime::native_oom::NativeAllocOom {
                length: num_fields,
                what: "object",
            })
    }

    /// [`Self::thread_plumbing_alloc`] for the mirror's pooled `name` String.
    pub(in crate::vm::vm_exec) fn thread_plumbing_string(
        &self,
        text: &str,
    ) -> Result<ObjectRef, crate::runtime::native_oom::NativeAllocOom> {
        super::try_create_java_string(self.shared, text).ok_or(
            crate::runtime::native_oom::NativeAllocOom {
                length: text.len(),
                what: "java/lang/String",
            },
        )
    }

    /// The slow half of `current_thread_object`: allocate, register and
    /// populate this thread's lazy `java.lang.Thread` mirror.
    ///
    /// gc-common w7-b (`common-f-g1-fatal-abort-from-infallible-alloc-object`
    /// row 2): every allocation here was infallible, so a native asking for
    /// `Thread.currentThread()` of a VM-attached thread on an exhausted heap
    /// aborted the process (Generational/ZGC) or drew down G1's reserve. Now,
    /// in an `unwind_ok()` context:
    ///
    /// * the mirror is allocated fallibly BEFORE it is registered, so a
    ///   refusal there changes nothing;
    /// * everything after the registration runs inside
    ///   `native_oom::catch_alloc_oom`, so a refusal of the name String, the
    ///   `FieldHolder` or any allocation the population reaches (the system
    ///   class loader's natives) ROLLS BACK: pins pushed since entry are
    ///   dropped and `java_thread_obj` is cleared, so the next call rebuilds
    ///   the mirror instead of serving a half-built one (`holder == null`).
    ///   The provisional registry and reverse parking bindings are withdrawn
    ///   too, so enumeration cannot publish an abandoned half-built mirror.
    ///
    /// Host callers also receive a refusal. Population always opens its own
    /// local OOM scope so a failed construction can roll back registration.
    ///
    /// `get_or_create_main_thread_group` (once per VM) and
    /// `ensure_thread_interrupt_lock` (one `Object` per thread) allocate
    /// through `heap_alloc_object` since the w7 orchestrator applied
    /// `handoff-w7b-thread-group-and-interrupt-lock-allocations`, so inside
    /// the `catch_alloc_oom` scope their refusal unwinds into this rollback
    /// like every other step's. (gc-common w8-b: this paragraph said they
    /// still took the infallible entry.)
    pub(in crate::vm::vm_exec) fn build_current_thread_object(
        &mut self,
    ) -> Result<ObjectRef, crate::runtime::native_oom::NativeAllocOom> {
        // Use the real java/lang/Thread class ID so virtual dispatch works.
        let class_id = {
            let cm = self.shared.classes.class_manager.read();
            cm.get_loaded_class_id("java/lang/Thread")
        }
        .unwrap_or_else(|| {
            // Thread class not loaded yet вЂ” load it now
            self.shared
                .classes
                .class_manager_write_settling()
                .load_class("java/lang/Thread")
                .unwrap_or(ClassId::new(0))
        });

        // In real-JDK mode the Thread class has many more than 3 instance
        // fields and real-JDK bytecode reads `this.holder.threadStatus`
        // etc.  Allocate with the full field count and populate
        // `name/tid/holder/priority` by resolved slot index.  Synthetic-JDK
        // mode still keeps the historical first three slots stable for
        // callers (name/priority/tid), but it must allocate at least the
        // synthetic class's declared field count too.  The synthetic Thread
        // layout now also declares `contextClassLoader`; allocating only 3
        // slots made Thread.get/setContextClassLoader hit an undersized
        // object as soon as reflection users resolved that field.
        let (is_real_jdk, num_fields) = {
            let cm = self.shared.classes.class_manager.read();
            match cm.class_store.get(class_id) {
                Some(c) if !c.origin.is_compatibility_stub() => (true, c.num_total_fields.max(3)),
                Some(c) => (false, c.num_total_fields.max(3)),
                _ => (false, 3usize),
            }
        };

        // BUG-03 (concurrent-spawn Thread.holder-null): this freshly-allocated
        // mirror is held in a Rust LOCAL across the allocating steps below
        // (`create_java_string`, `build_thread_field_holder` → ThreadGroup +
        // Reference.<clinit>, the system ClassLoader). Under GC stress a moving
        // young GC fires inside one of those allocations and RELOCATES the mirror:
        // the per-thread `java_thread_obj` field (and the registry mirror) are
        // remapped by `update_all_roots`/`update_thread_objs_after_gc`, but a bare
        // Rust local is NOT a GC root, so it goes stale. Using the stale local for
        // the `set_field(holder/name/…)` calls then writes those fields to the OLD
        // (zeroed) address — leaving the live mirror with `holder == null` — and the
        // function returns the stale ref. `Thread.<init>`'s
        // `currentThread().getThreadGroup()` then NPEs ("this.holder is null").
        // FIX: keep `thread_obj` in sync with the GC-remapped field by re-reading it
        // after every allocating step (the field is the authoritative live address).
        //
        // gc-common w7-b: fallible, and before the registration below, so a
        // refusal here leaves no trace.
        let thread_obj = self.thread_plumbing_alloc(class_id, num_fields)?;
        if is_real_jdk {
            crate::runtime::interpreter::init_primitive_fields(self.shared, thread_obj, class_id);
        }

        // Register immediately so any recursive call to
        // `current_thread_object` during holder/group construction
        // observes the in-progress object and doesn't loop. The mirror's
        // Java `tid` is usually still 0 here (ctor not run yet) — the
        // tid-guarded lookup path backfills it on first use.
        self.thread.java_thread_obj = Some(thread_obj);
        let java_tid = read_java_thread_tid(self.shared, thread_obj).unwrap_or(0);
        self.shared
            .threads
            .thread_registry
            .set_java_thread_obj_with_tid(self.thread.thread_id, thread_obj, java_tid);

        let pin_base = self.thread.native_pin_roots.len();
        let populated = crate::runtime::native_oom::catch_alloc_oom(|| {
            self.populate_current_thread_object(class_id, is_real_jdk, thread_obj)
        })
        .and_then(|r| r);
        if populated.is_err() {
            // Roll back (see the doc above): a half-built mirror must not be
            // served by the next `currentThread()`.
            self.thread.native_pin_roots.truncate(pin_base);
            if let Some(failed) = self.thread.java_thread_obj.take() {
                self.shared.threads.thread_registry
                    .clear_java_thread_obj_if(self.thread.thread_id, failed);
            }
        }
        populated
    }

    /// Everything `build_current_thread_object` does after registering the
    /// mirror: its `name`, `tid`, `priority`, `holder`, `group`,
    /// `contextClassLoader` and `interruptLock`. The caller opens a local
    /// OOM scope and rolls registration back on refusal.
    pub(in crate::vm::vm_exec) fn populate_current_thread_object(
        &mut self,
        class_id: ClassId,
        is_real_jdk: bool,
        mut thread_obj: ObjectRef,
    ) -> Result<ObjectRef, crate::runtime::native_oom::NativeAllocOom> {
        // Interpreter round i4 wave 84, lane L6: a JNI-attached thread that
        // `AttachCurrentThread` gave no name takes the JDK's
        // `Thread.genThreadName()` (HotSpot's `allocate_threadObj` runs
        // `Thread(ThreadGroup, Runnable)` for it), and its registry entry and
        // `JvmThread` take that name too. Its placeholder was `Thread-<id>`.
        let jdk_name = if is_real_jdk
            && crate::native::jni::take_foreign_default_name(self.thread.thread_id)
        {
            self.mint_jdk_default_thread_name()
        } else {
            None
        };
        let name_str = match jdk_name {
            Some(name) => name,
            None => self.thread_plumbing_string(&self.thread.name)?,
        };
        // Re-sync after the string allocation (may have moved the mirror).
        thread_obj = self.thread.java_thread_obj.unwrap_or(thread_obj);

        if is_real_jdk {
            let (name_slot, tid_slot, holder_slot, priority_slot, group_slot) = {
                let cm = self.shared.classes.class_manager.read();
                (
                    resolve_field_index_in_hierarchy(class_id, "name", &cm.class_store),
                    resolve_field_index_in_hierarchy(class_id, "tid", &cm.class_store),
                    resolve_field_index_in_hierarchy(class_id, "holder", &cm.class_store),
                    resolve_field_index_in_hierarchy(class_id, "priority", &cm.class_store),
                    resolve_field_index_in_hierarchy(class_id, "group", &cm.class_store),
                )
            };
            if let Some(slot) = name_slot {
                self.shared
                    .mem
                    .heap
                    .set_field(thread_obj, slot, Value::Object(Some(name_str)));
            }
            if let Some(slot) = tid_slot {
                // TID-COLLISION FIX (2026-07-07, ES testAllEqual
                // IllegalMonitorStateException/stall): Java-constructed
                // threads get `Thread.tid` from `ThreadIdentifiers.next()`
                // (whose counter, living in the Unsafe static-long side
                // store, currently starts at 0 → tids 0,1,2,…), while this
                // lazy VM mirror path fabricated `vm_id.max(1)` — TWO
                // numbering authorities whose ranges overlap. main (vm id 0
                // → tid 1) collided with the second Java-created thread
                // (tid 1), so tid-keyed algorithms confused the two:
                // `ReentrantReadWriteLock$Sync`'s `cachedHoldCounter.tid ==
                // LockSupport.getThreadId(current)` check let DIFFERENT
                // threads share one hold counter, corrupting read-lock hold
                // counts (the "attempt to unlock read lock" throw / leaked
                // read counts stalling the lock). Reproduced standalone with
                // `TidProbe` (threadId(): main=1, spawned=0,1,2 — duplicate
                // 1).
                //
                // Wave 82 (`i81-L6-main-thread-id-is-2-pow-40-20261113.md`):
                // that fix moved VM-built mirrors to `(1 << 40) + vm id`, so
                // `main` read larger than every thread the program creates,
                // where HotSpot 25 gives it `Thread.PRIMORDIAL_TID` (3). Now
                // ONE authority again, the JDK's own: the primordial thread
                // (registry id 0) takes `PRIMORDIAL_JAVA_TID`, which the
                // `ThreadIdentifiers` counter is seeded past, and any other
                // VM-built mirror (JNI-attached, VM-owned) mints from that
                // counter, as HotSpot's attached `Thread.<init>` does.
                let tid = if self.thread.thread_id.0 == 0 {
                    cratonvm_native_builtins::PRIMORDIAL_JAVA_TID
                } else {
                    cratonvm_native_builtins::mint_vm_thread_java_tid(self.shared.vm_identity)
                };
                self.shared
                    .mem
                    .heap
                    .set_field(thread_obj, slot, Value::Long(tid));
            }
            if let Some(slot) = priority_slot {
                // In JDK 19+ priority lives on FieldHolder, but older
                // (or stubbed) Thread layouts keep the outer field.
                self.shared
                    .mem
                    .heap
                    .set_field(thread_obj, slot, Value::Int(5));
            }
            if let Some(slot) = holder_slot {
                if let Some(holder) = self.build_thread_field_holder()? {
                    // build_thread_field_holder allocates the FieldHolder + the
                    // ThreadGroup and triggers Reference.<clinit> — the moving GC
                    // that relocates the mirror. Re-sync before writing `holder`, or
                    // it lands on the stale (old) address and the live mirror keeps
                    // holder==null (the BUG-03 crash).
                    thread_obj = self.thread.java_thread_obj.unwrap_or(thread_obj);
                    self.shared
                        .mem
                        .heap
                        .set_field(thread_obj, slot, Value::Object(Some(holder)));
                    if let Some(group_slot) = group_slot {
                        let holder_group = {
                            let cm = self.shared.classes.class_manager.read();
                            let holder_class = self.shared.mem.heap.class_id_of(holder);
                            resolve_field_index_in_hierarchy(holder_class, "group", &cm.class_store)
                                .map(|slot| self.shared.mem.heap.get_field(holder, slot))
                        };
                        if let Some(Value::Object(Some(group))) = holder_group {
                            self.shared.mem.heap.set_field(
                                thread_obj,
                                group_slot,
                                Value::Object(Some(group)),
                            );
                        }
                    }
                }
            }
            if let Some(slot) = group_slot {
                if let Some(group) = self.get_or_create_main_thread_group() {
                    // InnocuousThread.<clinit> reads Thread.group via Unsafe,
                    // bypassing Thread.getThreadGroup()/holder.group.
                    thread_obj = self.thread.java_thread_obj.unwrap_or(thread_obj);
                    self.shared
                        .mem
                        .heap
                        .set_field(thread_obj, slot, Value::Object(Some(group)));
                }
            }
            // C9: initialize contextClassLoader so SLF4J and other users
            // of Thread.getContextClassLoader() see the app ClassLoader
            // rather than null.  The JDK's getContextClassLoader() is a
            // plain Java method that returns this field directly, so a
            // null field means ServiceLoader's internal getResources()
            // call will NPE.
            let ccl_slot = {
                let cm = self.shared.classes.class_manager.read();
                resolve_field_index_in_hierarchy(class_id, "contextClassLoader", &cm.class_store)
            };
            if let Some(slot) = ccl_slot {
                // Use the real-JDK-layout system classloader (from
                // classloader_real) rather than the synthetic one, so
                // JDK bytecode reading ClassLoader fields by name sees
                // valid values rather than our synthetic Int(LOADER_APP=2).
                if let Ok(Some(loader)) =
                    cratonvm_native_builtins::classloader_real::get_or_create_system_cl(self)
                {
                    // System-CL construction allocates → may relocate the mirror.
                    thread_obj = self.thread.java_thread_obj.unwrap_or(thread_obj);
                    self.shared
                        .mem
                        .heap
                        .set_field(thread_obj, slot, Value::Object(Some(loader)));
                }
            }
            // INTERRUPTLOCK: seed `Thread.interruptLock` so the real-JDK
            // `Thread.blockedOn`'s `synchronized (interruptLock)` (driven by
            // every `AbstractInterruptibleChannel` begin/end) never NPEs on a
            // VM-synthesized Thread object that never ran `<init>`. See
            // `ensure_thread_interrupt_lock`.
            // Final re-sync: ensure_thread_interrupt_lock allocates a lock object,
            // and the returned ref must be the live (post-GC) mirror.
            thread_obj = self.thread.java_thread_obj.unwrap_or(thread_obj);
            self.ensure_thread_interrupt_lock(thread_obj);
            // SBR round 2 (lane `thread`): this mirror stands for a thread that
            // is already running (the primordial thread, a JNI-attached or
            // Rust-owned thread), so it is alive for the JDK's `Thread.alive()`
            // (`eetop != 0`) and RUNNABLE for `threadState()`.
            thread_obj = self.thread.java_thread_obj.unwrap_or(thread_obj);
            set_thread_state_fields(
                self.shared,
                thread_obj,
                Some(live_eetop(self.thread.thread_id)),
                Some(THREAD_STATUS_RUNNABLE),
            );
            return Ok(self.thread.java_thread_obj.unwrap_or(thread_obj));
        }

        // Synthetic-JDK fixed layout: name=0, priority=1, tid=2. `create_java_string`
        // above may have relocated the mirror; re-sync from the GC-remapped field
        // (same BUG-03 hazard as the real-JDK path).
        thread_obj = self.thread.java_thread_obj.unwrap_or(thread_obj);
        self.shared
            .mem
            .heap
            .set_field(thread_obj, 0, Value::Object(Some(name_str)));
        self.shared.mem.heap.set_field(thread_obj, 1, Value::Int(5)); // NORM_PRIORITY
        self.shared
            .mem
            .heap
            .set_field(thread_obj, 2, Value::Long(self.thread.thread_id.0 as i64));
        Ok(thread_obj)
    }

    /// `Thread.genThreadName()` -- `"Thread-" + ThreadNumbering.next()`, the
    /// JDK's counter -- for a JNI-attached thread that was given no name
    /// (interpreter round i4 wave 84, lane L6), renaming this thread's
    /// `JvmThread` and registry entry to it. `None` when the JDK has no such
    /// method or the call fails: the placeholder name stays. The answer is
    /// returned unpinned; the caller stores it before anything allocates.
    pub(in crate::vm::vm_exec) fn mint_jdk_default_thread_name(&mut self) -> Option<ObjectRef> {
        let name_obj = match invoke_static_shared(
            self.shared,
            self.thread,
            "java/lang/Thread",
            "genThreadName",
            "()Ljava/lang/String;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(s)))) => s,
            _ => return None,
        };
        let name = crate::vm::vm_object::read_java_string(&self.shared.mem.heap, name_obj)?;
        self.shared
            .threads
            .thread_registry
            .set_thread_name(self.thread.thread_id, &name);
        self.thread.name = name;
        Some(name_obj)
    }

    /// Build a `java.lang.Thread$FieldHolder` populated with sensible
    /// defaults for a VM-created thread: `group = <main ThreadGroup>`,
    /// `task = null`, `stackSize = 0`, `priority = NORM_PRIORITY (5)`,
    /// `daemon = <this JvmThread's daemon bit>`.  Returns `Ok(None)` if the FieldHolder class is
    /// missing or its constructor fails (e.g. synthetic-JDK runs where
    /// the inner class is absent), and `Err` when the heap cannot hold the
    /// holder (see [`Self::thread_plumbing_alloc`]; gc-common w7-b: this used
    /// the infallible entry, which aborts on Generational/ZGC and takes G1's
    /// reserve).
    pub(crate) fn build_thread_field_holder(
        &mut self,
    ) -> Result<Option<ObjectRef>, crate::runtime::native_oom::NativeAllocOom> {
        let Ok(holder_class) = <Self as NativeClassAccess>::ensure_class_initialized(
            self,
            "java/lang/Thread$FieldHolder",
        ) else {
            return Ok(None);
        };
        let holder_num_fields = {
            let cm = self.shared.classes.class_manager.read();
            cm.class_store
                .get(holder_class)
                .map(|c| c.num_total_fields)
                .unwrap_or(0)
        };
        if holder_num_fields == 0 {
            return Ok(None);
        }
        // Before any pin is pushed, so an `Err` leaves nothing to unwind.
        let holder = self.thread_plumbing_alloc(holder_class, holder_num_fields)?;
        crate::runtime::interpreter::init_primitive_fields(self.shared, holder, holder_class);

        // BUG-03 (concurrent-spawn): `holder` is a bare Rust local held across
        // `get_or_create_main_thread_group()` (allocates the ThreadGroup + triggers
        // Reference.<clinit> → a moving young GC) and the FieldHolder.<init> invoke.
        // A moving GC would relocate `holder` while the local kept the old address;
        // FieldHolder.<init> would then write `group`/priority/etc. to the STALE
        // holder, leaving the LIVE holder with `group == null` (so the parent's
        // `getThreadGroup()` returns null → `g.getMaxPriority()` NPE). Pin it across
        // the allocations (the GC remaps `native_pin_roots` in place) and read the
        // live address back. Watermark/LIFO-truncate per the native-pin convention.
        let pin_base = self.thread.native_pin_roots.len();
        self.thread.native_pin_roots.push(holder);

        let group = self.get_or_create_main_thread_group();
        // Re-read the (GC-remapped) holder after the group allocation.
        let holder = self.thread.native_pin_roots[pin_base];
        // `group` (whether freshly built by this call or served from the
        // `main_thread_group` cache — including the claim/wait/notify path's
        // "another thread is building, wait then re-read" branch) is a bare
        // Rust local from here on. `SharedVm::main_thread_group` is itself a
        // GC root now (memory/roots.rs step 8d / memory/gc.rs step 6d), so
        // the CACHE stays live and gets remapped by any later GC — but this
        // local `Copy` of the `ObjectRef` does not track that remap. Pin it
        // alongside `holder` so the invoke below (which runs `<init>`
        // bytecode and can itself trigger further moving GCs) can't leave it
        // stale; confirmed live via a `cratonvm-aio-dispatch-N` SIGSEGV
        // (`is_forwarded`/`get_header:1558`) inside the `FieldHolder.<init>`
        // field-setter that stores this exact value into `holder.group`.
        let group_idx = group.map(|g| {
            let idx = self.thread.native_pin_roots.len();
            self.thread.native_pin_roots.push(g);
            idx
        });
        let group = group_idx.map(|idx| self.thread.native_pin_roots[idx]);
        // r13w3-ffm2: the mirror inherits the thread's own daemon bit. A thread
        // attached as a daemon (JNI `AttachCurrentThreadAsDaemon`, an FFM upcall
        // on a foreign thread) builds its `java.lang.Thread` lazily here, and a
        // hard-coded `false` made `isDaemon()` lie for every one of them (and
        // made the threads they create non-daemon, since `Thread.<init>` copies
        // `parent.isDaemon()`). Kill switch `CRATONVM_THREAD_MIRROR_DAEMON=0`
        // restores the old constant `false`; read in both modes.
        let daemon = self.thread.daemon;
        let args = [
            Value::Object(Some(holder)),
            Value::Object(group), // ThreadGroup (may be None if group alloc failed)
            Value::Object(None),  // Runnable task
            Value::Long(0),       // stackSize
            Value::Int(5),        // priority = NORM_PRIORITY
            Value::Int(i32::from(daemon)),
        ];
        let init_result = invoke_on_class_shared(
            self.shared,
            self.thread,
            holder_class,
            "<init>",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;JIZ)V",
            &args,
        );
        // Read the live holder back (the invoke may have GC'd), then unpin.
        let holder = self
            .thread
            .native_pin_roots
            .get(pin_base)
            .copied()
            .unwrap_or(holder);
        self.thread.native_pin_roots.truncate(pin_base);
        if init_result.is_err() {
            return Ok(None);
        }
        Ok(Some(holder))
    }

    /// Lazily create (and cache on `SharedVm`) the "main" ThreadGroup.
    /// Uses the private no-arg `ThreadGroup()` constructor (the one the
    /// real JDK uses to build the root "system" group before
    /// `VM.isBooted()`), then patches the `name` field from "system" to
    /// "main".  The 4-arg `(ThreadGroup, String, int, boolean)` ctor
    /// cannot be called with `parent = null` because its `isBooted()`
    /// branch dereferences `parent.synchronizedAddWeak`.  The public
    /// `(String)` ctor would recurse through
    /// `Thread.currentThread().getThreadGroup()` вЂ” our caller is the
    /// thread construction path itself, so that would loop.
    ///
    /// CONCURRENCY (fixes a confirmed TOCTOU race — see
    /// CRATONVM-SPRING-GENUINE-BUGLIST, "5.8
    /// follow-up #4"): the body below allocates two `ThreadGroup` objects
    /// and runs their `<init>` (bytecode, can trigger a moving GC), so it
    /// cannot simply hold `main_thread_group`'s write lock across the
    /// whole build — `RwLock` is not reentrant, and this same function is
    /// reachable indirectly from inside a `<clinit>`/`<init>` it runs, and
    /// concurrent GC-blocked-region waits must not stall holding a plain
    /// lock across a safepoint. Instead this mirrors the codebase's own
    /// JVMS §5.5 class-init claim/wait/notify idiom
    /// (`vm_util::ensure_class_initialized_shared`,
    /// `SharedVm::class_init_waiters`): exactly one thread transitions
    /// `main_thread_group_init` `Idle -> InProgress` and performs the
    /// build; every other concurrent caller blocks on the `InProgress`
    /// claim's condvar — via the same deposit/enter_blocked/
    /// arrive_and_wait/check_post_block_gc protocol class-init waiters use
    /// — until the builder finishes and notifies, then re-checks the
    /// (now-published) result instead of racing its own independent build.
    pub(crate) fn get_or_create_main_thread_group(&mut self) -> Option<ObjectRef> {
        use std::sync::Arc;
        if let Some(obj) = *self.shared.threads.main_thread_group.read() {
            return Some(obj);
        }
        let current_thread_id = self.thread.thread_id.0;
        loop {
            // Try to claim the build under the (cheap, never held across an
            // allocation) init-state lock; discover an in-progress build by
            // someone else; or discover the result already landed while we
            // were retrying.
            let waiter = {
                let mut init = self.shared.threads.main_thread_group_init.lock();
                match &*init {
                    super::MainThreadGroupInit::InProgress {
                        owner_thread,
                        waiter,
                    } => {
                        if *owner_thread == current_thread_id {
                            // Re-entrant call on the SAME thread that is
                            // already building the group (e.g. a nested
                            // currentThread() triggered from within
                            // ThreadGroup's own <clinit>/<init>). Mirrors
                            // JVMS §5.5 step 2's re-entrant rule for class
                            // init ("release LC and complete normally"):
                            // return None (no group yet) rather than
                            // deadlocking on our own claim.
                            return None;
                        }
                        Some(Arc::clone(waiter))
                    }
                    super::MainThreadGroupInit::Idle => {
                        // Re-check under the lock: another thread may have
                        // finished (or failed) a build between our
                        // lock-free fast-path read above and acquiring
                        // this lock.
                        if let Some(obj) = *self.shared.threads.main_thread_group.read() {
                            return Some(obj);
                        }
                        *init = super::MainThreadGroupInit::InProgress {
                            owner_thread: current_thread_id,
                            waiter: Arc::new((
                                parking_lot::Mutex::new(false),
                                parking_lot::Condvar::new(),
                            )),
                        };
                        None
                    }
                }
            };

            let Some(waiter) = waiter else {
                // We claimed the build — proceed to the code below.
                break;
            };

            // Someone else is building; block until they finish. Same
            // GC-safe blocked-region protocol as class-init waiters: retire
            // the TLAB, deposit a root snapshot, arrive-and-wait if a STW
            // is already in flight so we don't strand a GC initiator, wait
            // on the condvar (bounded, so a builder that somehow never
            // notifies can't wedge us forever), then re-sync any
            // cross-GC fixups before touching heap state again.
            self.thread.tlab.retire();
            self.deposit_root_snapshot();
            let blk = self.shared.mem.gc_barrier.enter_blocked();
            if blk.pre_stw {
                let _ = self
                    .shared
                    .mem
                    .gc_barrier
                    .arrive_and_wait_auto(ThreadId(current_thread_id));
            }
            {
                let (lock, cvar) = &*waiter;
                let mut guard = lock.lock();
                if !*guard {
                    let _ = cvar.wait_for(&mut guard, std::time::Duration::from_secs(30));
                }
            }
            drop(blk);
            self.check_post_block_gc();
            // Loop back to re-check: `Some` once the builder published a
            // result; `Idle` (no result) if the builder failed, in which
            // case we retry and may become the new builder ourselves.
            if let Some(obj) = *self.shared.threads.main_thread_group.read() {
                return Some(obj);
            }
        }

        // We are now the sole builder. This guard runs on EVERY exit path
        // below (success, an early `None` return via `?`/`return`, or a
        // panic unwind) so a builder that bails out never leaves waiters
        // blocked for the full 30s timeout — or, worse, permanently wedges
        // `main_thread_group_init` in `InProgress` for a thread that has
        // already moved on.
        struct FinishGuard<'s> {
            shared: &'s SharedVm,
        }
        impl Drop for FinishGuard<'_> {
            fn drop(&mut self) {
                let prev = std::mem::replace(
                    &mut *self.shared.threads.main_thread_group_init.lock(),
                    super::MainThreadGroupInit::Idle,
                );
                if let super::MainThreadGroupInit::InProgress { waiter, .. } = prev {
                    let (lock, cvar) = &*waiter;
                    *lock.lock() = true;
                    cvar.notify_all();
                }
            }
        }
        let _finish_guard = FinishGuard {
            shared: self.shared,
        };

        let tg_class =
            <Self as NativeClassAccess>::ensure_class_initialized(self, "java/lang/ThreadGroup")
                .ok()?;
        let tg_num_fields = {
            let cm = self.shared.classes.class_manager.read();
            cm.class_store
                .get(tg_class)
                .map(|c| c.num_total_fields)
                .unwrap_or(0)
        };
        if tg_num_fields == 0 {
            return None;
        }
        // Build the root "system" group via the `private ThreadGroup()`
        // no-arg constructor — the one real-JDK uses to create the root
        // "system" group before `VM.isBooted()` returns true. It sidesteps
        // the `synchronizedAddWeak`/`synchronizedAddStrong` NPE path the
        // `(ThreadGroup, String, int, boolean)` ctor would take with a null
        // parent, and leaves the group named "system".
        // BUG-03 (concurrent-spawn): the group/string objects below are bare Rust
        // locals held across their `<init>` invokes and `create_java_string` — each
        // of which can trigger a moving young GC (Reference.<clinit> etc.) that
        // relocates them. Pin every such object on the native-pin watermark so the
        // GC remaps it in place, and re-read the live address from the pin after
        // each GC-capable step (a stale local would make the ctor write fields to
        // the old address → the live group ends up with parent/name == null).
        let pin_base = self.thread.native_pin_roots.len();

        // gc-common w7-b: fallible inside a native callback; on the unwind the
        // builder's `FinishGuard` resets `main_thread_group_init`.
        let system_tg = self.heap_alloc_object(tg_class, tg_num_fields);
        crate::runtime::interpreter::init_primitive_fields(self.shared, system_tg, tg_class);
        let sys_idx = self.thread.native_pin_roots.len();
        self.thread.native_pin_roots.push(system_tg);
        let sys_recv = self.thread.native_pin_roots[sys_idx];
        let init_ok = invoke_on_class_shared(
            self.shared,
            self.thread,
            tg_class,
            "<init>",
            "()V",
            &[Value::Object(Some(sys_recv))],
        )
        .is_ok();
        if !init_ok {
            self.thread.native_pin_roots.truncate(pin_base);
            return None;
        }

        // Build the conventional "main" group as a CHILD of "system" via the
        // public `ThreadGroup(ThreadGroup parent, String name)` constructor
        // (parent is non-null here, so the `synchronizedAddWeak` NPE path is
        // not taken). This mirrors HotSpot's `system <- main` hierarchy.
        //
        // Why the parent matters: the JDK's `InnocuousThread.createThreadGroup()`
        // walks `group.getParent()` up to the ROOT and creates
        // "InnocuousThreadGroup" as a child of that root. When "main" had no
        // parent (was itself the root), the cleaner/innocuous group landed as a
        // child of "main", so `ThreadGroup.enumerate(main, recurse=true)` —
        // which is exactly what randomizedtesting's `ThreadLeakControl.getThreads`
        // calls on the main thread group — swept up the JDK's `Common-Cleaner`
        // and per-`Cleaner` `Cleaner-N` daemon threads and reported them as
        // leaked/zombie in EVERY Elasticsearch `ESTestCase`. With "main" parented
        // under "system", `InnocuousThreadGroup` becomes a SIBLING of "main"
        // (child of "system"), invisible to `enumerate(main, …)` — matching
        // HotSpot, where these cleaner threads never trip the leak detector.
        let main_tg = self.heap_alloc_object(tg_class, tg_num_fields);
        crate::runtime::interpreter::init_primitive_fields(self.shared, main_tg, tg_class);
        let main_idx = self.thread.native_pin_roots.len();
        self.thread.native_pin_roots.push(main_tg);
        // zr z8/nr: inside a native a refused intern raises the catchable
        // `OutOfMemoryError` (the funnel truncates the pins) instead of the
        // infallible constructor's process abort (`fatal_string_heap_exhausted`);
        // outside one it is the same infallible call as before.
        let main_str = if crate::runtime::native_oom::unwind_ok() {
            match super::try_create_java_string(self.shared, "main") {
                Some(s) => s,
                None => crate::runtime::native_oom::raise(4, "String"),
            }
        } else {
            super::create_java_string(self.shared, "main")
        };
        let str_idx = self.thread.native_pin_roots.len();
        self.thread.native_pin_roots.push(main_str);
        // Re-read all three (GC-remapped) before the ctor invoke.
        let m_recv = self.thread.native_pin_roots[main_idx];
        let m_parent = self.thread.native_pin_roots[sys_idx];
        let m_name = self.thread.native_pin_roots[str_idx];
        let ctor_ok = invoke_on_class_shared(
            self.shared,
            self.thread,
            tg_class,
            "<init>",
            "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V",
            &[
                Value::Object(Some(m_recv)),
                Value::Object(Some(m_parent)),
                Value::Object(Some(m_name)),
            ],
        )
        .is_ok();
        // Re-read after the invoke (it may have GC'd).
        let system_tg = self.thread.native_pin_roots[sys_idx];
        let main_tg = self.thread.native_pin_roots[main_idx];
        let main_str = self.thread.native_pin_roots[str_idx];
        // Fallback: if the public ctor is unavailable, populate parent/name
        // directly so the hierarchy (and thus InnocuousThreadGroup placement)
        // is still correct.
        if !ctor_ok {
            let (parent_slot, name_slot) = {
                let cm = self.shared.classes.class_manager.read();
                (
                    resolve_field_index_in_hierarchy(tg_class, "parent", &cm.class_store),
                    resolve_field_index_in_hierarchy(tg_class, "name", &cm.class_store),
                )
            };
            if let Some(slot) = parent_slot {
                self.shared
                    .mem
                    .heap
                    .set_field(main_tg, slot, Value::Object(Some(system_tg)));
            }
            if let Some(slot) = name_slot {
                self.shared
                    .mem
                    .heap
                    .set_field(main_tg, slot, Value::Object(Some(main_str)));
            }
        }
        self.thread.native_pin_roots.truncate(pin_base);
        *self.shared.threads.main_thread_group.write() = Some(main_tg);
        Some(main_tg)
    }

    /// Seed `Thread.interruptLock` with a fresh `Object` if the slot is not
    /// already a reference. Real `java.lang.Thread` declares
    /// `final Object interruptLock = new Object();` as a field initializer in
    /// the `Thread(ThreadGroup,String,int,Runnable,long)` constructor, but that
    /// complex ctor does not run to completion for app-created threads in our
    /// VM, leaving the slot at its zero default (which reads back as `Int(0)`,
    /// not `Object(None)`, because `alloc_object` zeroes every slot regardless
    /// of declared type). `Thread.blockedOn(Interruptible)` does
    /// `synchronized (interruptLock)` on EVERY `AbstractInterruptibleChannel`
    /// begin/end (every FileChannel / SocketChannel op), so a null lock makes
    /// `monitorenter` throw NPE and takes down the interruptible-channel
    /// subsystem — e.g. H2's MVStore background writer on a file database.
    /// Idempotent: a no-op once the slot holds a real reference. Resolves the
    /// field through the receiver's own class hierarchy, so it works for Thread
    /// subclasses (BackgroundWriterThread, ForkJoinWorkerThread, …) too.
    pub(crate) fn ensure_thread_interrupt_lock(&mut self, thread_obj: ObjectRef) {
        let class_id = self.shared.mem.heap.class_id_of(thread_obj);
        let slot = {
            let cm = self.shared.classes.class_manager.read();
            resolve_field_index_in_hierarchy(class_id, "interruptLock", &cm.class_store)
        };
        let Some(slot) = slot else { return };
        if matches!(
            self.shared.mem.heap.get_field(thread_obj, slot),
            Value::Object(Some(_))
        ) {
            return;
        }
        let obj_class = {
            let cm = self.shared.classes.class_manager.read();
            cm.get_loaded_class_id("java/lang/Object")
        }
        .or_else(|| {
            self.shared
                .classes
                .class_manager_write_settling()
                .load_class("java/lang/Object")
                .ok()
        });
        if let Some(obj_class) = obj_class {
            // BUG-03 (concurrent-spawn): `thread_obj` is a bare param held across the
            // lock allocation below, which can trigger a moving GC that relocates it.
            // Pin it so the GC remaps it in place, then write the lock to the LIVE
            // address (a stale `thread_obj` would drop the write → null interruptLock).
            let pin_base = self.thread.native_pin_roots.len();
            self.thread.native_pin_roots.push(thread_obj);
            // gc-common w7-b: the fallible entry inside a native callback (an
            // unwind is caught by the funnel, which truncates the pins).
            let lock = self.heap_alloc_object(obj_class, 0);
            let thread_obj = self.thread.native_pin_roots[pin_base];
            self.thread.native_pin_roots.truncate(pin_base);
            self.shared
                .mem
                .heap
                .set_field(thread_obj, slot, Value::Object(Some(lock)));
        }
    }
}

impl NativeContextImpl<'_> {
    /// Write `holder.threadStatus` of THIS thread's own `Thread` mirror (SBR
    /// round 2, lane `thread`). Called by the two ends of every blocking
    /// region: `deposit_root_snapshot` (the state `GcBlockState::java_state`
    /// names) just before the thread publishes `in_blocked_region`, and
    /// `check_post_block_gc` (RUNNABLE) once the wake has applied its fixups.
    /// Both run on the owning thread as a scanned mutator, so the plain int
    /// store is safe; `java_thread_obj` is the GC-remapped address.
    ///
    /// The slot pair is resolved once per `JvmThread`. Allocates nothing. A
    /// mirror without a built `holder` (under construction, or a synthetic
    /// stub) is left alone and nothing is cached.
    pub(crate) fn publish_thread_status(&self, status: i32) {
        // Wave 101 (lane L6): a main thread whose `java.lang.Thread` ended
        // (`end_main_thread_java_side`) keeps TERMINATED through the
        // launcher's later waits.
        if self
            .shared
            .threads
            .thread_registry
            .java_side_ended(self.thread.thread_id)
        {
            return;
        }
        // A JDK continuation blocked while mounted (a pinned `Object.wait`, a
        // contended `monitorenter`) is its CARRIER blocked, as on HotSpot,
        // where the two are one `JavaThread`: `VirtualThread.threadState()`
        // of a RUNNING virtual thread answers `carrier.threadState()`, and
        // the registry, JMX and JVMTI read the carrier's `java_state` (the
        // carrier waits in `enterSpecial`, GC-blocked; `super::jdk_continuation`).
        let carrier = self.thread.jdk_continuation.carrier();
        let mirror = if self.thread.jdk_continuation.jdk() && carrier != 0 {
            let registry = &self.shared.threads.thread_registry;
            let java_state = if status == THREAD_STATUS_RUNNABLE {
                0
            } else {
                self.thread
                    .gc_block_state
                    .java_state
                    .load(std::sync::atomic::Ordering::Acquire)
            };
            if let Some(state) = registry.gc_block_state_of(ThreadId(carrier)) {
                state
                    .java_state
                    .store(java_state, std::sync::atomic::Ordering::Release);
            }
            registry.java_thread_obj(ThreadId(carrier))
        } else {
            self.thread.java_thread_obj
        };
        let Some(mirror) = mirror else {
            return;
        };
        let slots = if self.thread.jdk_continuation.jdk() {
            // The carrier's mirror, not this thread's own: resolved each time
            // (a virtual thread's own mirror has no `holder` to cache from).
            thread_status_slots_of(self.shared, mirror)
        } else {
            self.thread_status_slots()
        };
        let Some((holder_slot, status_slot)) = slots else {
            return;
        };
        let heap = &self.shared.mem.heap;
        if let Value::Object(Some(holder)) = heap.get_field(mirror, holder_slot) {
            heap.set_field(holder, status_slot, Value::Int(status));
        }
    }

    /// `(Thread.holder slot, Thread$FieldHolder.threadStatus slot)` of this
    /// thread's mirror, resolved once per `JvmThread` and cached in
    /// `JvmThread::thread_status_slots`; `None` while the mirror has no built
    /// `holder` (nothing is cached then). A running mutator only (it reads the
    /// mirror on the first call).
    pub(crate) fn thread_status_slots(&self) -> Option<(usize, usize)> {
        use std::sync::atomic::Ordering;
        let packed = self.thread.thread_status_slots.load(Ordering::Relaxed);
        if packed != 0 {
            return Some(unpack_thread_status_slots(packed));
        }
        let mirror = self.thread.java_thread_obj?;
        let heap = &self.shared.mem.heap;
        let cm = self.shared.classes.class_manager.read();
        let thread_class = cm.get_loaded_class_id("java/lang/Thread")?;
        let holder_slot =
            resolve_field_index_in_hierarchy(thread_class, "holder", &cm.class_store)?;
        let Value::Object(Some(holder)) = heap.get_field(mirror, holder_slot) else {
            return None;
        };
        let status_slot = resolve_field_index_in_hierarchy(
            heap.class_id_of(holder),
            "threadStatus",
            &cm.class_store,
        )?;
        drop(cm);
        self.thread.thread_status_slots.store(
            (1u64 << 63) | ((holder_slot as u64) << 32) | status_slot as u64,
            Ordering::Relaxed,
        );
        Some((holder_slot, status_slot))
    }
}

/// `(Thread.holder slot, Thread$FieldHolder.threadStatus slot)` of `mirror`,
/// uncached; `None` while it has no built `holder`.
pub(in crate::vm::vm_exec) fn thread_status_slots_of(shared: &SharedVm, mirror: ObjectRef) -> Option<(usize, usize)> {
    let heap = &shared.mem.heap;
    let cm = shared.classes.class_manager.read();
    let thread_class = cm.get_loaded_class_id("java/lang/Thread")?;
    let holder_slot = resolve_field_index_in_hierarchy(thread_class, "holder", &cm.class_store)?;
    let Value::Object(Some(holder)) = heap.get_field(mirror, holder_slot) else {
        return None;
    };
    let status_slot = resolve_field_index_in_hierarchy(
        heap.class_id_of(holder),
        "threadStatus",
        &cm.class_store,
    )?;
    Some((holder_slot, status_slot))
}

/// The `(holder slot, threadStatus slot)` pair packed in
/// `JvmThread::thread_status_slots` (non-zero).
#[inline]
pub(in crate::vm::vm_exec) fn unpack_thread_status_slots(packed: u64) -> (usize, usize) {
    (
        ((packed >> 32) & 0x7fff_ffff) as usize,
        (packed & 0xffff_ffff) as usize,
    )
}

/// `Object.wait`'s [`crate::threading::monitor::WaitReacquireReport`]: a
/// waiter whose re-acquisition has to queue behind another owner is BLOCKED
/// in both words `Thread.getState()` can be answered from -- the
/// `GcBlockState::java_state` word (the registry, JVMTI, JDWP) and the
/// mirror's `holder.threadStatus` (the JDK's own `Thread.threadState()`,
/// since SBR round 2 retired the `getState` native).
///
/// Interpreter round i2 wave 54, lane L7
/// (`docs/feature-designs/real-jdk-mode.md`;
/// probe `L7W29TWaitNotify.java` rows 3-4): the deposit
/// at the start of the wait wrote WAITING into `threadStatus`, and nothing
/// rewrote it for the re-acquire, so the JDK read WAITING where HotSpot says
/// BLOCKED.
///
/// The waiter is GC-blocked here (excluded from every pause's census), so its
/// own `java_thread_obj` may be stale: the mirror is read from the thread
/// registry (remapped by every collection) and the write is made inside
/// `GcBarrier::with_no_pause_in_progress`, where no pause can relocate the
/// mirror or its holder under it -- the rule the JNI monitor ops of an
/// excluded thread follow. Lock order barrier -> registry, as the census's.
/// Only the contended re-acquire calls this; `check_post_block_gc` writes
/// RUNNABLE once the wait has returned.
pub(crate) struct WaitReenterStatus<'a> {
    pub(crate) shared: &'a SharedVm,
    pub(crate) tid: ThreadId,
    pub(crate) java_state: &'a std::sync::atomic::AtomicU8,
    /// `NativeContextImpl::thread_status_slots`, resolved before the wait
    /// (`None`: no holder to write; the state word alone is set).
    pub(crate) slots: Option<(usize, usize)>,
}

impl WaitReenterStatus<'_> {
    /// `holder.threadStatus = BLOCKED_ON_MONITOR_ENTER` through the registry's
    /// (remapped) mirror. The caller rules out a moving pause meanwhile.
    pub(in crate::vm::vm_exec) fn write_blocked_status(&self, holder_slot: usize, status_slot: usize) {
        let shared = self.shared;
        let Some(mirror) = shared.threads.thread_registry.java_thread_obj(self.tid) else {
            return;
        };
        let heap = &shared.mem.heap;
        let (mirror, valid) = heap.load_and_forward_checked(mirror);
        if !valid {
            return;
        }
        if let Value::Object(Some(holder)) = heap.get_field(mirror, holder_slot) {
            heap.set_field(
                holder,
                status_slot,
                Value::Int(THREAD_STATUS_BLOCKED_ON_MONITOR_ENTER),
            );
        }
    }
}

impl crate::threading::monitor::WaitReacquireReport for WaitReenterStatus<'_> {
    /// JIT round 25 wave 8, lane W8-3
    /// (`docs/feature-designs/real-jdk-mode.md`):
    /// the waiter owns the monitor again, so the wait state word goes to
    /// RUNNABLE now -- `ThreadRegistry::java_block_state` reads it for a
    /// GC-blocked waiter, and for a running (lazy) one only while it is
    /// WAITING / TIMED_WAITING / BLOCKED -- and a `ThreadInfo` read inside a
    /// frame-trace pause that holds the waiter before it leaves the native no
    /// longer pairs TIMED_WAITING with the waited monitor owned (30-244 of
    /// ~2 000 `R25WaitingSampleShape` wait samples on host c15/c16, every
    /// tier). One store under the monitor's state lock; the holder's
    /// `threadStatus` is restored by the wait's own exit, as before.
    fn reacquired(&self) {
        self.java_state
            .store(0, std::sync::atomic::Ordering::Release);
    }

    fn blocked_on_reenter(&self) {
        use crate::threading::monitor::WaitReacquireReport;
        self.java_state.blocked_on_reenter();
        let Some((holder_slot, status_slot)) = self.slots else {
            return;
        };
        let shared = self.shared;
        shared
            .mem
            .gc_barrier
            .with_no_pause_in_progress(|| self.write_blocked_status(holder_slot, status_slot));
    }

    /// The lazy wait's running re-acquisition (`MonitorTuning::wait_lazy_reacquire`):
    /// the same two words, written as the running mutator this thread still
    /// is -- no pause can move the mirror while it runs (every pause waits for
    /// it), and waiting one out here would be that pause's missing arrival.
    fn blocked_on_reenter_running(&self) {
        use crate::threading::monitor::WaitReacquireReport;
        self.java_state.blocked_on_reenter();
        let Some((holder_slot, status_slot)) = self.slots else {
            return;
        };
        {
            self.write_blocked_status(holder_slot, status_slot);
        }
    }

    /// Interpreter round i2 wave 55, lane L6
    /// (`docs/feature-designs/real-jdk-mode.md`):
    /// the `Object.wait` twin of the contended entry's give-back. One load
    /// while no suspension is in force; never without `experimental-debug`.
    #[inline]
    fn reentry_held_off(&self) -> bool {
        #[cfg(feature = "experimental-debug")]
        {
            crate::debug::monitor_entry_held_off(self.shared, self.tid)
        }
        #[cfg(not(feature = "experimental-debug"))]
        {
            false
        }
    }

    /// The entry's give-back loop (`debug::exit_monitor_while_suspended`) on
    /// the fresh hold. BLOCKED (`MONITOR`) meanwhile is the monitor's call
    /// (`Monitor::wait` reports it for a notified or contended re-entry only;
    /// interpreter round i2 wave 57, lane L6).
    #[cold]
    #[inline(never)]
    fn give_back_while_held_off(
        &self,
        monitor: &crate::threading::monitor::Monitor,
        thread_id: ThreadId,
    ) {
        #[cfg(feature = "experimental-debug")]
        crate::debug::exit_monitor_while_suspended(self.shared, thread_id, monitor);
        #[cfg(not(feature = "experimental-debug"))]
        let _ = (monitor, thread_id);
    }

    /// Interpreter round i2 wave 56, lane L6
    /// (`CRATONVM_MONITOR_SUSPENSION_AWARE_ENTRY`): the waiter answers to the
    /// debugger's hold itself, and the monitor runs its one give-back loop on
    /// the fresh re-entry. Never without `experimental-debug`.
    #[inline]
    fn entry_hold(&self) -> Option<&dyn crate::threading::monitor::EntryHold> {
        #[cfg(feature = "experimental-debug")]
        if self.shared.threads.monitors.suspension_aware_entry() {
            return Some(self);
        }
        None
    }
}

/// The wait re-entry's [`crate::threading::monitor::EntryHold`]: the
/// debugger's (`debug::DebuggerEntryHold`), reached through the waiter's VM.
#[cfg(feature = "experimental-debug")]
impl crate::threading::monitor::EntryHold for WaitReenterStatus<'_> {
    #[inline]
    fn held_off(&self, thread_id: ThreadId) -> bool {
        crate::debug::monitor_entry_held_off(self.shared, thread_id)
    }

    fn wait_until_released(&self, thread_id: ThreadId) {
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[MONITOR_ENTRY_HOLD] tid={} wait re-entry given back",
                thread_id.0
            );
        }
        crate::threading::monitor::EntryHold::wait_until_released(
            &crate::debug::DebuggerEntryHold {
                shared: self.shared,
            },
            thread_id,
        );
    }
}

