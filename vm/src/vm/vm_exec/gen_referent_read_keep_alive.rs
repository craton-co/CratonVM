#[allow(unused_imports)]
use super::*;

/// gcj j1/rf (`g1-and-generational-referent-reads-through-unsafe-and-jni-have-no-keep-alive-20261019`,
/// the Generational half) — HotSpot's `ON_UNKNOWN_OOP_REF` /
/// `jni_GetObjectField` referent keep-alive on the Generational heap: a read
/// of instance slot `slot` of `receiver` (an `Unsafe.getReference[Volatile]`,
/// a JNI `GetObjectField`, a reflective `Field.get`, a field `MethodHandle` or
/// `VarHandle`) that hands `referent` to Java logs it through the SATB
/// barrier, as `Reference.get()` does (`gc_reference_keep_alive`), when
///
/// 1. the heap is Generational (G1 and ZGC: nothing here, unchanged);
/// 2. a concurrent old-generation mark is open (`VmHeap::satb_armed`: one
///    Acquire load, the common case's whole cost) -- that mark hides slot 0
///    of every old-generation `Reference` it was handed at initial mark
///    (`ConcurrentMarker::set_reference_skip`, under
///    `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`, default ON), so a referent read
///    and parked in an object the marker already scanned is reachable only
///    through an edge the snapshot cannot see; the remark would clear its
///    weak reference and the sweep free it while it is strongly reachable;
/// 3. `slot` is 0, the slot the mark hides (`Reference.referent`, the first
///    instance field `java.lang.ref.Reference` declares);
/// 4. `receiver`'s class is a soft / weak / phantom reference class (or a
///    `Cleaner`): `ref_discovery::receiver_is_reference`, the test compiled
///    `getfield` already applies (`jit_getfield_body`, SBR4 satb) and the
///    classes whose slot 0 the mark can hide -- HotSpot's
///    `java_lang_ref_Reference::is_referent_field(base, offset)`. Slot 0 of
///    any other object is not hidden, so it gets no floating garbage (the
///    ZGC slot-0 rule's approximation is not copied here).
///
/// `CRATONVM_GEN_REFERENT_READ_KEEP_ALIVE` (default ON, `=0` kill: dev's raw
/// read) is read once per process, after the cheap gates.
pub(crate) fn gen_referent_read_keep_alive(
    shared: &SharedVm,
    receiver: ObjectRef,
    slot: usize,
    referent: ObjectRef,
) {
    let heap = &shared.mem.heap;
    if slot != 0 || !heap.is_generational() || !heap.satb_armed() {
        return;
    }
    if !gen_referent_read_keep_alive_enabled()
        || !crate::memory::ref_discovery::receiver_is_reference(shared, heap.class_id_of(receiver))
    {
        return;
    }
    heap.satb_barrier(Value::Object(Some(referent)));
}

/// gcj j1/rf — `CRATONVM_GEN_REFERENT_READ_KEEP_ALIVE` (default ON, `=0`
/// kill), memoised (one relaxed load; a test's flag override resets it).
pub(in crate::vm::vm_exec) fn gen_referent_read_keep_alive_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// What `NativeContextImpl::touch_soft_reference`'s lock-free fast path needs
/// about one VM, remembered per thread. gc-common w6-d (2026-09-24).
pub(in crate::vm::vm_exec) struct SoftTouchMemo {
    /// The VM this was resolved for (`SharedVm::vm_identity`, never reused).
    pub(in crate::vm::vm_exec) vm_identity: usize,
    /// [`cratonvm_classloading::class_origin_epoch`] at resolution: a class
    /// definition or upgrade re-resolves the layout. Under
    /// `CRATONVM_INTERP_CLASS_MEMO_NARROW_EPOCH` (wave 95), [`class_id_memo_epoch`]
    /// when `layout` is `Some`.
    pub(in crate::vm::vm_exec) class_epoch: u64,
    /// `java.lang.ref.SoftReference`'s class id and the slot of the
    /// `long timestamp` it declares itself; `None` when this VM's
    /// `SoftReference` has no such field (the synthetic shape, or not loaded
    /// yet), which keeps every touch on the locked path.
    pub(in crate::vm::vm_exec) layout: Option<(ClassId, usize)>,
    /// The receiver class this thread last proved a `SoftReference` subclass
    /// (valid while `class_epoch` holds: a class id is re-issued only by a
    /// class definition, which moves the epoch).
    pub(in crate::vm::vm_exec) verified_cid: Option<ClassId>,
    /// The VM's reference processor's soft index epoch
    /// (`ReferenceProcessor::soft_touch_epoch`).
    pub(in crate::vm::vm_exec) epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// gcj j4/rf: its published touch clock (`ReferenceProcessor::soft_touch_clock`,
    /// `0` unless `CRATONVM_SOFTREF_TOUCH_KEYED_ON_CLOCK`) and the mutator
    /// clock a skipped touch raises (`soft_touch_observed_ms`).
    pub(in crate::vm::vm_exec) clock: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(in crate::vm::vm_exec) observed: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

std::thread_local! {
    /// One VM's worth of [`SoftTouchMemo`] per thread. A cache, not state: a
    /// miss (another VM, a class definition since) re-resolves under the
    /// ClassManager and the processor lock. Keyed by the never-reused
    /// `vm_identity`, so two VMs in one process never share an answer.
    pub static SOFT_TOUCH_MEMO: std::cell::RefCell<Option<SoftTouchMemo>> =
        const { std::cell::RefCell::new(None) };
}

/// `java.lang.ref.SoftReference`'s class id, and the instance slot of the
/// `long timestamp` field it DECLARES ITSELF — never a subclass's field of the
/// same name, which a by-name resolution on the receiver's class would find
/// first (`class CacheRef extends SoftReference { long timestamp; }` is an
/// ordinary thing to write). `None` for any other shape.
pub(in crate::vm::vm_exec) fn soft_reference_timestamp_layout(shared: &SharedVm) -> Option<(ClassId, usize)> {
    let cm = shared.classes.class_manager.read();
    let soft_cid = cm.find_bootstrap_class_by_name("java/lang/ref/SoftReference")?;
    let class = cm.class_store.get(soft_cid)?;
    let mut instance_offset = 0;
    for field in &class.fields {
        if field.is_static() {
            continue;
        }
        if &*field.name == "timestamp" && &*field.descriptor == "J" {
            return Some((soft_cid, class.first_field_index + instance_offset));
        }
        instance_offset += 1;
    }
    None
}

/// The raw `long` in `obj`'s slot `slot`, or `None` when it does not read as
/// one.
pub(in crate::vm::vm_exec) fn soft_timestamp_field(shared: &SharedVm, obj: ObjectRef, slot: usize) -> Option<i64> {
    match shared.mem.heap.get_field_as(obj, slot, b'J') {
        Value::Long(v) => Some(v),
        _ => None,
    }
}

/// Is this VIRTUAL thread's continuation pinned by a native that re-entered
/// Java beneath the running native (or by a pinning Rust door)? A yield would
/// unwind that frame and the remount would never run its post-call work, so
/// the thread blocks its carrier instead, as HotSpot pins a continuation with
/// a native frame on it (interpreter round i1 wave 26, lane L4;
/// `native_callee_memo::continuation_pinned_by_native`). A `<clinit>` pins
/// the same way: `vm_util`'s initializer runs it in a nested Rust call and
/// finalizes the class state after it returns, which a yield would skip (the
/// class stays `Initializing`); HotSpot pins a virtual thread blocked in a
/// class initializer too. Read only by the yield decisions below and
/// `vt_pin_count`; a platform thread is never pinned.
/// A virtual thread the VM schedules itself (a `ThreadBuilders$BoundVirtualThread`
/// on the VM's carrier pool, `VmConfig::jdk_continuations` off), as opposed to
/// a JDK continuation's thread (`JvmThread::jdk_continuation`): the JDK owns
/// that one's parks, permits and pinned-park events (`VirtualThread.park`,
/// `parkOnCarrierThread`), so a VM park of it is a plain park of its carrier
/// (whose `ParkState` and interrupt flag it shares while mounted).
#[inline]
pub(crate) fn vm_scheduled_virtual(thread: &JvmThread) -> bool {
    matches!(thread.kind, crate::threading::ThreadKind::Virtual) && !thread.jdk_continuation.jdk()
}

#[inline]
pub(in crate::vm::vm_exec) fn vt_pinned_by_native(thread: &JvmThread) -> bool {
    matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && (crate::runtime::native_callee_memo::continuation_pinned_by_native()
            || super::vm_util::in_clinit_shared())
}

/// gcd d6/f: the native stack a Java platform thread's carrier gets when the
/// program asked for nothing (`Thread.stackSize == 0`, no `-Xss`): Rust's 8 MiB
/// default, or `RUST_MIN_STACK`. Unchanged from before d6/f.
pub(in crate::vm::vm_exec) const DEFAULT_CARRIER_STACK_BYTES: usize = 8 * 1024 * 1024;

/// gcd d6/f: the largest stack REQUEST honoured, HotSpot's own ceiling for
/// `-Xss` / `ThreadStackSize` on 64-bit (1 GiB). A larger request (up to
/// `Long.MAX_VALUE` is legal Java) is clamped to it rather than handed to the
/// OS, which would refuse the reservation and fail the spawn.
pub(in crate::vm::vm_exec) const MAX_REQUESTED_THREAD_STACK_BYTES: u64 = 1 << 30;

/// gcd d6/f: a grown carrier stack is rounded up to this (a page multiple on
/// every target).
pub(in crate::vm::vm_exec) const CARRIER_STACK_ROUNDING: usize = 1 << 20;

/// gcd d6/f (`docs/feature-designs/real-jdk-mode.md`):
/// the carrier stack for a platform thread that requested `requested` bytes
/// (`Thread.holder.stackSize`, or the `-Xss` default when that is 0), given the
/// VM's default `default_bytes`.
///
/// HotSpot treats the request as a hint and honours it. CratonVM's frames are
/// larger than HotSpot's (a compiled frame about 4x, an interpreted level a
/// few KiB of Rust recursion), and the VM runs its own Rust frames under the
/// first Java frame, so a request the default already covers keeps the default
/// (a thread asking for 256 KiB still gets 8 MiB, byte-for-byte as before), and
/// a larger one gets the request ON TOP of the default budget: `requested +
/// DEFAULT_CARRIER_STACK_BYTES`, rounded up to 1 MiB, the request clamped to
/// [`MAX_REQUESTED_THREAD_STACK_BYTES`]. Never smaller than `default_bytes`.
/// A request `<= 0` is "none".
pub(in crate::vm::vm_exec) fn carrier_stack_bytes(default_bytes: usize, requested: i64) -> usize {
    let Ok(requested) = u64::try_from(requested) else {
        return default_bytes;
    };
    let requested =
        usize::try_from(requested.min(MAX_REQUESTED_THREAD_STACK_BYTES)).unwrap_or(default_bytes);
    if requested <= default_bytes {
        return default_bytes;
    }
    let wanted = requested.saturating_add(DEFAULT_CARRIER_STACK_BYTES);
    let rounded = wanted.div_ceil(CARRIER_STACK_ROUNDING) * CARRIER_STACK_ROUNDING;
    rounded.max(default_bytes)
}

/// JIT round 23 wave 2, lane L3
/// (`docs/feature-designs/real-jdk-mode.md`;
/// `CRATONVM_JIT_MAIN_THREAD_XSS_BUDGET`, correctness, default ON, `=0` keeps
/// `0`): the compiled self-recursion budget growth
/// (`JvmThread::self_call_extra_budget`) the MAIN thread gets for a `-Xss`
/// default (`VmConfig::default_thread_stack_size`). The same growth a platform
/// thread started with no `stackSize` gets from that default
/// (`carrier_stack_bytes` minus the default carrier), so `-Xss16m` deepens
/// compiled recursion on `main` as it does on every other thread and as
/// HotSpot's `-Xss` does for its main thread. `None` (no `-Xss`, every
/// embedding default) answers `0`, byte for byte as before. Pure.
pub(crate) fn main_thread_self_call_extra_budget(
    default_thread_stack_size: Option<usize>,
) -> usize {
    let Some(bytes) = default_thread_stack_size.map(|b| i64::try_from(b).unwrap_or(i64::MAX))
    else {
        return 0;
    };
    carrier_stack_bytes(DEFAULT_CARRIER_STACK_BYTES, bytes)
        .saturating_sub(DEFAULT_CARRIER_STACK_BYTES)
}

/// gcd d6/f: the stack size `thread_obj` was constructed with --
/// `new Thread(group, target, name, stackSize)` stores it in
/// `Thread.holder.stackSize` (JDK 19+'s `Thread$FieldHolder`); an older layout
/// with `stackSize` on `Thread` itself is read too. `0` when absent (a
/// synthetic `Thread`, a holder not wired yet) or not a `long`.
pub(in crate::vm::vm_exec) fn read_thread_stack_size_request(shared: &SharedVm, thread_obj: ObjectRef) -> i64 {
    let heap = &shared.mem.heap;
    let cm = shared.classes.class_manager.read();
    // Round 15 wave 4 (lane threadvm): `holder` (and the older layout's
    // `stackSize`) are the fields `java.lang.Thread` declares
    // (`thread_field_owner`); by name a subclass's own `holder` lost the
    // request, or supplied one. `Thread$FieldHolder` is final, so its
    // `stackSize` is exact from the holder's own class.
    let class_id = thread_field_owner(
        shared,
        &cm.class_store,
        heap.get_header(thread_obj).class_id,
    );
    let (owner, owner_class) =
        match resolve_field_index_in_hierarchy(class_id, "holder", &cm.class_store) {
            Some(holder_slot) => match heap.get_field(thread_obj, holder_slot) {
                Value::Object(Some(holder)) => (holder, heap.get_header(holder).class_id),
                _ => return 0,
            },
            None => (thread_obj, class_id),
        };
    match resolve_field_index_in_hierarchy(owner_class, "stackSize", &cm.class_store)
        .map(|slot| heap.get_field(owner, slot))
    {
        Some(Value::Long(n)) => n,
        _ => 0,
    }
}


impl<'a> NativeThreadAccess for NativeContextImpl<'a> {

    fn monitor_enter(&mut self, obj: ObjectRef) {
        // Gated diagnostic only (CRATONVM_DBG_MONENTER, default OFF): deposit
        // this thread's frame snapshot so the contended-`enter` poll loop can
        // emit it if a watchdog stack-dump fires while we are blocked here.
        // No cost in normal runs — the flag is read once and cached.
        let dbg_mon_dump = crate::threading::monitor::mon_enter_dump_enabled();
        if dbg_mon_dump {
            crate::vm::vm_init::set_wait_site_snapshot(&*self.thread);
        }
        // NOTE deliberately NOT monitor_enter_blocking: the calling native
        // holds raw `Value` copies (its args slice) that are neither rooted
        // nor remappable across a GC-blocked wait; block as an EXPECTED
        // mutator instead (a concurrent STW waits for us).
        //
        // STW-TAKEOVER-FIX (2026-07-13) follow-up: this reasoning IS the
        // root cause of the WildFly `parallel-extension-add` STW-barrier
        // deadlock (see
        // `wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md`)
        // for the one call site proven live via gdb to hit it
        // (`CountDownLatch`'s polling loop). An earlier version of this fix
        // switched THIS method wholesale to `monitor_enter_blocking`, but a
        // full audit of every native caller of `monitor_enter` (~80 sites:
        // Semaphore/Phaser/Exchanger/blocking-queue/ConcurrentHashMap/
        // ReentrantLock/Condition/BouncyCastle-random/etc.) found the
        // overwhelming majority keep using the SAME `obj`/`this` afterward
        // without any pin-and-refresh — switching all of them to a
        // GC-pausable wait at once would trade one hang for a batch of new,
        // unaudited stale-`ObjectRef`-across-GC bugs (this codebase's most
        // recurring defect class, see
        // `wildfly-parallel-boot-stale-objectref-residual.md`).
        // `monitor_enter` therefore stays on this original, non-GC-blocked
        // path for everyone; `monitor_enter_gc_safe` (below) is the narrow,
        // opt-in escape hatch for the one call site with live evidence.
        self.shared
            .threads
            .monitors
            .enter(obj, self.thread.thread_id);
        if dbg_mon_dump {
            crate::vm::vm_init::clear_wait_site_snapshot();
        }
        // JEP 491: a virtual thread that holds a monitor is pinned to its
        // carrier and cannot be unmounted. Track the pin depth so that
        // subsequent park/sleep calls can emit `jdk.VirtualThreadPinned`.
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual) {
            self.thread.pin_count = self.thread.pin_count.saturating_add(1);
            self.thread.pin_reason = "Monitor";
            note_unstructured_monitor_enter(self.shared, self.thread.thread_id);
        } else {
            // Wave 99 (lane L6): a platform thread's native enter counts too,
            // on its own `JvmThread` (`JvmThread::native_monitor_enters`).
            self.thread.native_monitor_enters = self.thread.native_monitor_enters.saturating_add(1);
        }
    }

    fn monitor_enter_gc_safe(&mut self, obj: ObjectRef) -> ObjectRef {
        // STW-TAKEOVER-FIX (2026-07-13): the narrow, opt-in counterpart to
        // `monitor_enter` above — see its doc comment and
        // `NativeContext::monitor_enter_gc_safe`'s trait doc for the full
        // story. Delegates to the already-established, bytecode-tested
        // `monitor_enter_blocking` (deposit root snapshot, retire TLAB,
        // `GcBarrier::enter_blocked`/`arrive_and_wait_auto`, `block_enter`,
        // `mark_blocked_region_leave`/`check_post_block_gc`, pin+remap the
        // object across the wait) and returns the current, possibly-
        // relocated reference — callers MUST use it for anything after this
        // call. The fast (uncontended) path is unaffected:
        // `monitor_enter_blocking` returns immediately, zero barrier
        // interaction, when `enter_or_contend` acquires without contention.
        let fixed = monitor_enter_blocking(self.shared, self.thread, obj);
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual) {
            self.thread.pin_count = self.thread.pin_count.saturating_add(1);
            self.thread.pin_reason = "Monitor";
            note_unstructured_monitor_enter(self.shared, self.thread.thread_id);
        } else {
            // Wave 99 (lane L6): as in `monitor_enter`.
            self.thread.native_monitor_enters = self.thread.native_monitor_enters.saturating_add(1);
        }
        fixed
    }

    /// gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    /// item 3): the binding a native dispatch installs (`JniContextGuard`),
    /// from this context's own `&mut`, for the length of `f`.
    fn run_with_jni_binding(&mut self, f: &mut dyn FnMut()) {
        let own: *mut JvmThread = &mut *self.thread;
        // SAFETY: derived from this context's exclusive borrow, which nothing
        // uses until the guard (which restores the previous binding) drops.
        let _guard = unsafe { JniContextGuard::install(self.shared, own) };
        f();
    }

    fn monitor_exit(&mut self, obj: ObjectRef) {
        // `monitor_enter_gc_safe` publishes JMX ownership (it goes through
        // `monitor_enter_blocking`), so this is its retract. Plain
        // `monitor_enter` above never publishes, which makes the retract a
        // harmless no-op there rather than a wrong removal - the list is
        // address-keyed and `remove` on an absent address does nothing.
        let exited = monitor_exit_and_retract_jmx(self.shared, obj, self.thread.thread_id).is_ok();
        if exited && matches!(self.thread.kind, crate::threading::ThreadKind::Virtual) {
            note_unstructured_monitor_exit(self.shared, self.thread.thread_id);
        } else if exited {
            // Wave 99 (lane L6): the platform count `monitor_enter` raised.
            self.thread.native_monitor_enters = self.thread.native_monitor_enters.saturating_sub(1);
        }
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual)
            && self.thread.pin_count > 0
        {
            self.thread.pin_count -= 1;
            if self.thread.pin_count == 0 {
                self.thread.pin_reason = "";
            }
        }
    }

    fn monitor_wait(&mut self, obj: ObjectRef, timeout_ms: Option<u64>) -> MethodCallResult {
        self.monitor_wait_with_state(obj, timeout_ms, false, false)
    }

    fn monitor_wait_reporting_timeout(
        &mut self,
        obj: ObjectRef,
        timeout_ms: Option<u64>,
    ) -> MethodCallResult {
        self.monitor_wait_with_state(obj, timeout_ms, true, false)
    }

    fn monitor_wait_as(
        &mut self,
        obj: ObjectRef,
        timeout_ms: Option<u64>,
        kind: cratonvm_native_api::BlockKind,
    ) -> MethodCallResult {
        use cratonvm_native_api::BlockKind;
        // Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`):
        // a served stand-in's wait reads the state of the JDK code it stands
        // for. Off, every such wait is the plain native `Object.wait` it was
        // (WAITING, `IN_OBJECT_WAIT`), whatever the kind.
        if !self.shared.threads.thread_registry.served_wait_kinds() {
            return self.monitor_wait_with_state(obj, timeout_ms, false, false);
        }
        let (timed, parked) = match kind {
            BlockKind::Parked => (false, true),
            BlockKind::TimedParked => (true, true),
            BlockKind::TimedWaiting | BlockKind::Sleeping | BlockKind::TimedObjectWait => {
                (true, false)
            }
            BlockKind::Waiting | BlockKind::InNative | BlockKind::ObjectWait => (false, false),
        };
        self.monitor_wait_with_state(obj, timeout_ms, timed, parked)
    }

    fn monitor_notify(&mut self, obj: ObjectRef) -> MethodCallResult {
        // Interpreter round i4 wave 98, lane L6: an unmounted virtual-thread
        // waiter it chose is marked notified and BLOCKED now, while this
        // thread still owns the monitor (HotSpot's `INotify`); `getState()`
        // answered WAITING until the release's hand-off.
        let mut moved = Vec::new();
        self.shared
            .threads
            .monitors
            .notify_reporting(obj, self.thread.thread_id, Some(&mut moved))?;
        mark_notified_virtual_waiters(self.shared, &moved);
        Ok(None)
    }

    fn monitor_notify_all(&mut self, obj: ObjectRef) -> MethodCallResult {
        let mut moved = Vec::new();
        self.shared
            .threads
            .monitors
            .notify_all_reporting(obj, self.thread.thread_id, Some(&mut moved))?;
        mark_notified_virtual_waiters(self.shared, &moved);
        Ok(None)
    }

    fn thread_start(&mut self, thread_obj: ObjectRef) -> MethodCallResult {
        // Spawning a child thread needs an owning `Arc<SharedVm>` to hand to it.
        // A `SharedVm` built directly (`Arc::new(SharedVm::new(config))`, the
        // shape unit fixtures use) has no weak self-reference — only `Vm::new()`
        // installs one — so `get_arc()` would abort the process here. Report the
        // missing threading substrate as an error instead: natives that spawn
        // workers (e.g. `StructuredTaskScope.fork`) already document an inline
        // fallback for "thread_start returned Err (no thread registry
        // available)", and that fallback cannot run if we panic first.
        let Some(shared_arc) = self.shared.try_get_arc() else {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "Thread.start: this VM cannot spawn threads (no `self_arc` \
                          self-reference installed; not built via Vm::new)"
                    .to_string(),
            }));
        };
        // Interpreter round i5 wave 100, lane L6 (page
        // `i100-L6-compatible-first-virtual-thread-group-is-null`): before a
        // bound virtual thread starts, initialize `Thread$Constants` (the
        // VirtualThreads group) here, on the starting thread, whose
        // `currentCarrierThread()` is a platform thread. The virtual thread
        // itself cannot (its `currentCarrierThread()` answers itself), and its
        // first `getThreadGroup()` answered null.
        let thread_obj = self.init_virtual_thread_group_before_start(thread_obj);
        let tid = self.shared.threads.thread_registry.next_thread_id();
        let header = self.shared.mem.heap.get_header(thread_obj);
        // gcd d6/f: `new Thread(.., stackSize)`, read while `thread_obj` is
        // fresh (nothing above allocates); `0` falls back to `-Xss` below.
        let requested_stack_bytes = read_thread_stack_size_request(self.shared, thread_obj);

        // Read thread name from the real-JDK `name` field, falling back to the
        // legacy synthetic slot 0 layout.
        // Round 15 wave 4 (lane threadvm): `name` (and `eetop` below) are the
        // fields `java.lang.Thread` declares (`thread_field_owner`); by name
        // a subclass's own `String name` named the registry entry (the JMX /
        // thread-dump name).
        let (field_owner, name_slot) = {
            let cm = self.shared.classes.class_manager.read();
            let owner = thread_field_owner(self.shared, &cm.class_store, header.class_id);
            (
                owner,
                resolve_field_index_in_hierarchy(owner, "name", &cm.class_store),
            )
        };
        let name_value = name_slot
            .map(|slot| self.shared.mem.heap.get_field(thread_obj, slot))
            .unwrap_or_else(|| self.shared.mem.heap.get_field(thread_obj, 0));
        let name = match name_value {
            Value::Object(Some(str_ref)) => crate::vm::vm_object::read_java_string(&self.shared.mem.heap, str_ref)
                .unwrap_or_else(|| format!("Thread-{}", tid.0)),
            _ => format!("Thread-{}", tid.0),
        };

        // Check if this is a virtual thread.
        //
        // Two paths matter here:
        //   1. Synthetic-mode `Thread`: the fabricated layout, with `is_virtual`
        //      at `SYNTHETIC_THREAD_VIRTUAL_SLOT` (set by the synthetic
        //      Thread.Builder.start native).
        //   2. Real-JDK mode (JEP 444): a `BoundVirtualThread` (or any
        //      future `VirtualThread`) extends `BaseVirtualThread`. We must
        //      detect this by walking the class hierarchy вЂ” the synthetic
        //      slot trick won't work because the real Thread layout puts
        //      different fields there. Without this detection, virtual
        //      threads created by `Thread.ofVirtual().start(r)` wouldn't
        //      release the carrier semaphore on `Thread.sleep`/`park`,
        //      starving the carrier pool under load (e.g. 10K vthreads).
        //
        // The synthetic read is gated on the receiver actually HAVING the
        // fabricated layout, asked by a field NAME the real class declares and
        // the stub does not (`eetop`). It used to be gated on
        // `num_slots() >= 5`, which every real `Thread` satisfies (19 fields),
        // so the read was landing on the real `contextClassLoader` and was
        // correct only because a reference can never `matches!` an `Int(1)`.
        // A slot count cannot identify a layout — that lesson is written down
        // in three other places in this tree.
        let is_virtual_synthetic = {
            const VSLOT: usize =
                cratonvm_native_builtins::jdk25_concurrency::SYNTHETIC_THREAD_VIRTUAL_SLOT;
            let fabricated = {
                let cm = self.shared.classes.class_manager.read();
                resolve_field_index_in_hierarchy(field_owner, "eetop", &cm.class_store).is_none()
            };
            fabricated
                && header.num_slots() as usize > VSLOT
                && matches!(
                    self.shared.mem.heap.get_field(thread_obj, VSLOT),
                    Value::Int(1)
                )
        };
        let is_virtual_real_jdk = {
            let cm = self.shared.classes.class_manager.read();
            // Look up `BaseVirtualThread`'s class id once. If not loaded
            // (synthetic-only run), this returns None and we skip the
            // hierarchy walk. The class is loaded the moment any
            // VirtualThread / BoundVirtualThread is constructed.
            cm.get_loaded_class_id("java/lang/BaseVirtualThread")
                .map(|base_id| cm.is_subclass_of(header.class_id, base_id))
                .unwrap_or(false)
        };
        let is_virtual = is_virtual_synthetic || is_virtual_real_jdk;
        let executor_worker = self.thread_pool_executor_worker_task(thread_obj);
        let is_executor_worker = executor_worker.is_some();

        // T19.K1 вЂ” read the Java-side daemon flag so the registry can
        // tell `wait_for_non_daemon_threads()` whether the process must
        // wait for this thread. The flag is layout-dependent: synthetic
        // Threads don't model `daemon` (the synthetic 5-slot Thread
        // layout doesn't have a daemon field, so we default to false);
        // real-JDK Threads keep it on `Thread.holder.daemon` (see
        // `java.lang.Thread$FieldHolder`). Virtual threads (JEP 444)
        // are always daemon per the spec вЂ” we set that unconditionally
        // so a buggy `BoundVirtualThread` constructor can't keep the VM
        // alive past `main()`. Registry-daemonize real ThreadPoolExecutor
        // workers too: Spring's contexts have already closed by main return,
        // but CratonVM's ExecutorService.shutdown shim can leave idle workers
        // parked in getTask()->take(), which should not hold the suite process.
        //
        // Round 15 wave 4 (lane threadvm;
        // `r15w4-threadvm-pool-workers-registered-daemon-FIXED-20260930.md`): that
        // is not HotSpot's answer. A pool worker is a daemon only if its
        // factory made it one, and HotSpot waits at exit for a non-daemon
        // worker still running a task (`submit(task); shutdown();` then `main`
        // returns). `CRATONVM_THREAD_POOL_WORKER_OWN_DAEMON` reads the
        // worker's own flag, as for any thread.
        //
        // Round 15 wave 5 (lane threadvm2): default ON (`=0` restores the
        // rule). The shim the rule papered over (5399b69b85, 2026-07-04: the
        // real-pool `shutdown()` bridge was a no-op, so idle workers stayed in
        // `getTask()`) now advances the run state, interrupts idle workers,
        // runs `onShutdown` and `tryTerminate`
        // (`transition_real_executor_to_shutdown`); under `--real-jdk` the real
        // `shutdown()` runs. The one pool that must stay daemon is the VM's
        // own async pool (`lang_misc::is_vm_async_pool_worker`): it stands in
        // for `ForkJoinPool.commonPool()`, whose workers HotSpot makes daemon,
        // and its factory threads are not. It is registered daemon and its
        // Java flag says so, as a common-pool worker's does.
        let executor_worker_forced_daemon = match executor_worker {
            None => false,
            Some(_)
                if false =>
            {
                true
            }
            Some(worker) => {
                let vm_async_pool =
                    cratonvm_native_builtins::lang_misc::is_vm_async_pool_worker(self, worker);
                if vm_async_pool {
                    self.set_thread_holder_daemon_flag(thread_obj);
                }
                vm_async_pool
            }
        };
        let is_daemon = if is_virtual || executor_worker_forced_daemon {
            true
        } else {
            self.read_thread_daemon_flag(thread_obj).unwrap_or(false)
        };

        // Register thread as alive before spawning. zt t15/pl6: a virtual
        // thread's entry is marked so (the JMM roster never lists it).
        if is_virtual {
            self.shared
                .threads
                .thread_registry
                .register_starting_virtual(tid, &name, thread_obj);
        } else {
            self.shared
                .threads
                .thread_registry
                .register_starting_with_daemon(tid, &name, Some(thread_obj), is_daemon);
        }
        // Interpreter round i5 wave 100, lane L6: a platform thread's start
        // counts in the JMM's started / live / peak counters, as HotSpot's
        // `ThreadService::add_thread`; a virtual thread's never does.
        if !is_virtual {
            self.shared
                .threads
                .thread_registry
                .note_jmm_thread_start(tid);
        }
        // Record the mirror's Java `Thread.tid` (fully constructed by
        // `start()` time) so identity lookups take the aliasing-proof tid
        // index instead of comparing the mirror's recyclable heap address.
        if let Some(java_tid) = read_java_thread_tid(self.shared, thread_obj) {
            self.shared
                .threads
                .thread_registry
                .set_java_thread_obj_with_tid(tid, thread_obj, java_tid);
        }
        // SBR round 2 (lane `thread`): the JDK's `Thread.alive()` is
        // `eetop != 0` and its `start()` / `threadState()` read
        // `holder.threadStatus`; HotSpot sets both before the native thread
        // runs (`JavaThread::prepare`). Nothing above allocates, so
        // `thread_obj` is still the live address.
        set_thread_state_fields(
            self.shared,
            thread_obj,
            Some(live_eetop(tid)),
            Some(THREAD_STATUS_RUNNABLE),
        );

        // Record JFR thread start event
        {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let mut jfr = self.shared.debug.flight_recorder.lock();
            // Round-9 HIGH-5: build the Arc once and hand ownership to the
            // `_arc` variant so the emit doesn't reallocate from `&str`.
            let name_arc: std::sync::Arc<str> = std::sync::Arc::from(name.as_str());
            cratonvm_jfr::builtin::emit_thread_start_event_arc(
                &mut jfr,
                name_arc,
                if is_virtual { "virtual" } else { "platform" },
                tid.0,
                now_ns,
            );
        }

        // SBR5 lane jfr: the real JFR backend's jdk.ThreadStart.
        if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
            let parent = self.current_thread_object();
            // `current_thread_object` may allocate: re-read the new thread's object
            // from the registry (remapped after every GC) rather than reuse the
            // argument.
            let child = self
                .shared
                .threads
                .thread_registry
                .java_thread_obj(tid)
                .unwrap_or(thread_obj);
            cratonvm_native_builtins_jfr::jfr_vm_events::thread_started(self, child, Some(parent));
        }

        // Fire JDWP ThreadStart event, telling a virtual thread apart
        // (interpreter round i1 wave 25: `PlatformThreadsOnly`). A platform
        // thread posts its own, on itself before its `run()` (interpreter
        // round i4 wave 85, lane L1: below, in the spawned closure), as
        // HotSpot's `JavaThread::run` posts JVMTI `ThreadStart`; it was
        // posted here, by the parent, so a `SUSPEND_ALL` stop the parent
        // reached right after `start()` could be reported first.
        #[cfg(feature = "experimental-debug")]
        if is_virtual {
            crate::debug::send_thread_event_of(
                &self.shared,
                crate::debug::events::EventKind::ThreadStart,
                tid.0 as u64,
                is_virtual,
            );
        }

        // Store the ThreadId on the Java Thread object so we can look it up later.
        //
        // Synthetic-JDK layout: name=0, priority=1, tid=2 вЂ” write directly.
        //
        // Real-JDK layout: `Thread` is loaded from java.base.jmod, has dozens
        // of instance fields, and does NOT keep `tid` at slot 2 (slot 2 in
        // the real layout is something else, often `priority` or part of an
        // inherited field set).  Writing `Long(tid)` to slot 2 in real-JDK
        // mode corrupts whatever object reference / int the JDK bytecode
        // expects there вЂ” observed as "expected object reference, got
        // double(...)" when AQS / ReentrantLock subsequently dereferenced
        // the field in a worker thread.
        //
        // For real-JDK Thread we instead rely on the registry's
        // `(ObjectRef в†’ ThreadId)` lookup, which was already populated by
        // the `register(tid, name, Some(thread_obj))` call above.  This
        // also matches how `find_park_state_by_thread_obj` works.
        let is_real_jdk_thread = {
            let cm = self.shared.classes.class_manager.read();
            cm.class_store
                .get(header.class_id)
                .map(|c| !c.origin.is_compatibility_stub())
                .unwrap_or(false)
        };
        if !is_real_jdk_thread && header.num_slots() >= 3 {
            self.shared
                .mem
                .heap
                .set_field(thread_obj, 2, Value::Long(tid.0 as i64));
        }

        // Pre-create shared state so that operations (like unpark) from the parent
        // thread work even before the child thread starts executing.
        //
        // Interpreter round i3 wave 74, lane L6 (page
        // `i73-L6-unpark-in-the-thread-start-window-is-lost-20261106`): the
        // state is the one `register_starting_with_daemon` made above, not a
        // second one. The `java_tid` index and `eetop` are live since then,
        // so an `unpark` (or `interrupt`) of this thread from another one may
        // already have reached that state; a fresh state here dropped its
        // permit and the thread's first park waited for the next unpark.
        //
        // Interpreter round i3 wave 82, lane L6
        // (`CRATONVM_THREAD_START_ONE_PUBLISH`, default off; page
        // `i74-L6-proposal-one-registry-write-per-thread-start-20261107`): a
        // platform thread takes EVERY `Arc` its entry holds in one read and
        // adopts them, so neither side re-installs them with a write lock.
        let start_shared = if !is_virtual
            && self
                .shared
                .threads
                .thread_registry
                .thread_start_one_publish()
        {
            self.shared
                .threads
                .thread_registry
                .starting_thread_shared(tid)
        } else {
            None
        };
        let (pre_park, pre_interrupted) = match &start_shared {
            Some(shared) => (shared.park_state.clone(), shared.interrupted.clone()),
            None => self
                .shared
                .threads
                .thread_registry
                .starting_thread_states(tid),
        };
        // Interpreter round i3 wave 79, lane L6 (page
        // `i74-L6-interrupt-before-start-is-not-seen-by-park-or-sleep`): an
        // interrupt given before `start()` lives only in the Java
        // `Thread.interrupted` field (JDK 25 `interrupt()` sets it, then
        // `interrupt0` finds no live thread). HotSpot reads that field, so
        // the started thread's first park returns and its first sleep
        // throws; seed the VM flag from it before the thread can run. A
        // concurrent interrupt that already reached the adopted flag set
        // both, so a store of `true` here is idempotent.
        // The JFR start event above may have allocated: read the mirror the
        // registry remaps, as that block does.
        let mirror = self
            .shared
            .threads
            .thread_registry
            .java_thread_obj(tid)
            .unwrap_or(thread_obj);
        if read_thread_interrupted_field(self.shared, mirror) {
            pre_interrupted.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // With `start_shared` the entry already holds exactly these `Arc`s
        // (and `register_starting_with_daemon` already enabled the atomic
        // permit and filled the reverse index), so the two write locks
        // below would re-install what is there.
        if start_shared.is_none() {
            self.shared
                .threads
                .thread_registry
                .set_park_state(tid, pre_park.clone());
            self.shared
                .threads
                .thread_registry
                .set_interrupted_flag(tid, pre_interrupted.clone());
        }

        // Seed `Thread.interruptLock` on the child's Thread object before it
        // runs: its AbstractInterruptibleChannel begin/end path calls
        // `Thread.blockedOn` -> `synchronized (interruptLock)`, which NPEs on a
        // null lock. Real `Thread.<init>` sets this, but the complex ctor the
        // field-initializer lives in does not run to completion for app-created
        // threads in our VM (e.g. H2 MVStore's background writer on a file DB).
        self.ensure_thread_interrupt_lock(thread_obj);
        if is_virtual {
            self.shared
                .threads
                .virtual_thread_manager
                .create_virtual_thread_with_id(tid.0, &name);
            let carrier_shared = shared_arc.clone();
            self.shared
                .threads
                .virtual_thread_manager
                .start_carriers_once(move |vt_id| {
                    resume_virtual_continuation(carrier_shared.clone(), vt_id);
                });
        }
        let thread_obj_for_spawn = thread_obj;
        // Round-9 misc HIGH fix (audit `round9-misc.md`): this knob sizes the
        // *native Rust stack* for the carrier OS thread that hosts a Java
        // thread — it is NOT the Java `-Xss` budget. CratonVM's interpreter
        // recurses in Rust at ~2-4 KiB per Java frame (recursive
        // `execute_frame` + invoke dispatch + the JIT entry trampoline), so a
        // 2 MiB native stack maps to roughly 500-1000 Java frames before
        // overflow. That blows up on real-world clinit cascades — Quarkus,
        // WildFly, and the JDK module loader all push past 1000 nested
        // <clinit> frames during cold start. Use Rust's 8 MiB default to give
        // the recursive interpreter enough headroom; this still honours
        // `RUST_MIN_STACK` for tests that want to dial it down.
        //
        // gcd d6/f: and a thread that ASKED for a stack gets one --
        // `new Thread(group, target, name, stackSize)`, or `-Xss` for a thread
        // that passed 0 (`VmConfig::default_thread_stack_size`), as HotSpot
        // does. `carrier_stack_bytes` keeps the default for no request or one
        // the default covers, and otherwise gives the request on top of the
        // default budget (clamped, rounded). Everything that derives this
        // thread's stack band reads the OS's answer for the carrier
        // (`shadow_stack::current_thread_stack`, the conservative-roots
        // limits, the JIT self-call floor behind `jit_stack_floor`), and the
        // `StackOverflowError` ceilings are derived from `child_stack_size`
        // right below (`init_thread_exec_depth_ceiling`), so all of them see
        // the real size. The interpreter's frame cap (`max_stack_depth`) and
        // the JIT's compiled self-recursion budget (`SELF_CALL_STACK_BUDGET`)
        // grows only by the carrier growth, through
        // `JvmThread::self_call_extra_budget` (set below).
        let default_stack_size = cratonvm_types::flags::runtime_var("RUST_MIN_STACK")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_CARRIER_STACK_BYTES);
        let requested_stack_bytes = if requested_stack_bytes > 0 {
            requested_stack_bytes
        } else {
            self.shared
                .config
                .default_thread_stack_size
                .and_then(|bytes| i64::try_from(bytes).ok())
                .unwrap_or(0)
        };
        let child_stack_size = carrier_stack_bytes(default_stack_size, requested_stack_bytes);
        // Interpreter round i1 wave 38, lane L7: the same request raises the
        // interpreter's frame limit (`FrameStack::at_frame_limit`, 8192 frames
        // per MiB; it only raises, and a virtual thread asks for none).
        // JIT round 23 wave 6, lane L3: the platform thread's Java stack size
        // (its `stackSize`, else `-Xss`; `0` = HotSpot's 1 MiB default), for
        // the compiled self-recursion budget (`jit::helpers::SelfCallBudget`:
        // 16 times it). Clamped like the carrier request.
        let self_call_java_stack_bytes = if is_virtual {
            0
        } else {
            u64::try_from(requested_stack_bytes)
                .ok()
                .map(|b| b.min(MAX_REQUESTED_THREAD_STACK_BYTES))
                .and_then(|b| usize::try_from(b).ok())
                .unwrap_or(0)
        };
        let requested_frame_limit =
            crate::runtime::frame::frame_limit_for_stack_bytes(if is_virtual {
                0
            } else {
                requested_stack_bytes
            });
        if is_virtual {
            let mut virtual_runtime = Box::new(JvmThread::new(tid, &name));
            // nio21 wave 3 L1: this VM's redefinitions only.
            virtual_runtime
                .invoke_cache
                .bind_redefinition_log(self.shared.classes.redefinition_log.clone());
            virtual_runtime.kind = crate::threading::ThreadKind::Virtual;
            virtual_runtime.park_state = pre_park;
            virtual_runtime.interrupted = pre_interrupted;
            virtual_runtime.java_thread_obj = self
                .shared
                .threads
                .thread_registry
                .java_thread_obj(tid)
                .or(Some(thread_obj_for_spawn));
            self.shared
                .threads
                .thread_registry
                .set_root_snapshot(tid, virtual_runtime.root_snapshot.clone());
            self.shared
                .threads
                .thread_registry
                .set_frame_trace(tid, virtual_runtime.frame_trace.clone());
            self.shared
                .threads
                .thread_registry
                .set_vm_state(tid, virtual_runtime.vm_state.clone());
            self.shared
                .threads
                .thread_registry
                .set_gc_block_state(tid, virtual_runtime.gc_block_state.clone());
            self.shared.threads.thread_registry.set_tlab_addr(
                tid,
                &virtual_runtime.tlab as *const cratonvm_gc::Tlab as usize,
            );
            self.shared
                .threads
                .thread_registry
                .set_jvm_thread_addr(tid, (&*virtual_runtime as *const JvmThread) as usize);
            self.shared
                .threads
                .virtual_thread_manager
                .install_runtime(tid.0, virtual_runtime);
            self.shared.threads.virtual_thread_manager.start(tid.0);
            std::thread::yield_now();
            return Ok(None);
        }
        let thread_name_for_builder = name.clone();
        // The handle the deferred-start door hands the spawn (the body below
        // moves `shared_arc`).
        let spawn_vm = std::sync::Arc::clone(&shared_arc);
        // Interpreter round i2 wave 63, lane L1: a C JVMTI agent's
        // `RunAgentThread` body, which runs in place of `run()`.
        #[cfg(feature = "experimental-debug")]
        let agent_body = crate::jvmti::native_env::take_pending_agent_body();
        // Phase 5 WP3: the body is built here and spawned through this VM's
        // deferred starts below, so a boot image's capture run keeps every
        // Java-visible effect of `start0` above (registry row, `eetop`,
        // `threadStatus`) but creates the OS thread only after the restore.
        let java_thread_builder = std::thread::Builder::new()
            .name(thread_name_for_builder.clone())
            .stack_size(child_stack_size);
        let java_thread_body = move || {
            // H6: record this carrier's native stack size so the interpreter's
            // re-entrant `execute` recursion guard derives a correct
            // StackOverflowError ceiling for it. Worker carriers default to an
            // 8 MiB native stack — far smaller than the 128 MiB main VM thread —
            // so without this the guard (calibrated for the larger stack) would
            // let deep re-entrant native dispatch overflow the 8 MiB stack and
            // abort the whole process uncatchably before tripping.
            crate::runtime::interpreter::init_thread_exec_depth_ceiling(child_stack_size);
            // `CRATONVM_THREAD_START_ONE_PUBLISH` (wave 82): adopt the entry's
            // `Arc`s, so the four `set_*` write locks below are skipped.
            let adopted_shared = start_shared.is_some();
            let mut jvm_thread = match start_shared {
                Some(shared) => JvmThread::with_shared(tid, &name, shared),
                None => JvmThread::new(tid, &name),
            };
            // nio21 wave 3 L1: this VM's redefinitions only.
            jvm_thread
                .invoke_cache
                .bind_redefinition_log(shared_arc.classes.redefinition_log.clone());
            // gcd d6/f item 2: the compiled self-recursion budget grows by what
            // the carrier was grown for this thread's request (0 otherwise).
            jvm_thread.self_call_extra_budget = child_stack_size.saturating_sub(default_stack_size);
            jvm_thread.self_call_java_stack_bytes = self_call_java_stack_bytes;
            jvm_thread.frames.set_requested_frame_limit(requested_frame_limit);
            // Use the pre-created shared state
            jvm_thread.park_state = pre_park;
            jvm_thread.interrupted = pre_interrupted;
            // CRIT (stale-ref) — `thread_obj_for_spawn` is a raw ObjectRef captured
            // on the PARENT at spawn and never remapped; a moving GC between the
            // capture and this worker's startup relocates the Thread mirror, leaving
            // the captured ref stale. The registry's `java_thread_obj` IS remapped
            // after every GC (`update_thread_objs_after_gc`), so read the current
            // address from there (the worker was `register`ed with this mirror on the
            // parent before spawn), falling back to the captured ref only if absent.
            // Mirrors the identical read-back already done at thread END (search
            // `wake_obj`).
            jvm_thread.java_thread_obj = Some(
                shared_arc
                    .threads.thread_registry
                    .java_thread_obj(tid)
                    .unwrap_or(thread_obj_for_spawn),
            );
            // NOTE: `is_virtual` is impossible inside this closure — the
            // `is_virtual` arm above returns before `std::thread::Builder`
            // is ever reached, so a virtual thread never gets an OS carrier
            // of its own. The `jvm_thread.kind = Virtual` assignment and the
            // `virtual_scheduler.acquire()` that used to live here were both
            // dead; the acquire additionally blocked on a permit pool
            // disjoint from the real carriers (see the removal note in
            // `vm/src/threading/virtual_scheduler.rs`).
            // Share root snapshot with registry for GC cross-thread access
            // (already shared when `adopted_shared`).
            if !adopted_shared {
                shared_arc
                    .threads.thread_registry
                    .set_root_snapshot(tid, jvm_thread.root_snapshot.clone());
                // Share the frame-trace slot too, so cross-thread
                // getStackTrace / dumpThreads can read this worker's parked
                // call stack.
                shared_arc
                    .threads.thread_registry
                    .set_frame_trace(tid, jvm_thread.frame_trace.clone());
            }
            // CRASH DIAGNOSTICS (`startup-and-diagnostics.md` §6.3): the same
            // handle, additionally published into THIS OS thread's local cell.
            // The registry copy serves cross-thread readers (getStackTrace,
            // dumpThreads); this one serves the crash handler, which runs on
            // the faulting thread and until now could only render the
            // primordial thread's frames — so a SIGSEGV or a panic on a worker
            // arrived with no Java context at all, which is precisely the shape
            // of the STW-takeover deadlock reports. TLS rather than a shared
            // registry keeps the read lock-free; the guard un-publishes when
            // this worker's closure returns.
            let _crash_frames = super::vm_util::PublishedFrameTrace::publish(
                &name,
                tid.0,
                jvm_thread.frame_trace.clone(),
            );
            if !adopted_shared {
                // Share the optional VM-side breadcrumb for STW diagnostics.
                shared_arc
                    .threads.thread_registry
                    .set_vm_state(tid, jvm_thread.vm_state.clone());
                // Share blocked-region GC state so initiators can maintain
                // this thread's roots while it parks in a blocking native.
                shared_arc
                    .threads.thread_registry
                    .set_gc_block_state(tid, jvm_thread.gc_block_state.clone());
            }
            // BUG-03 — publish this worker's TLAB address so the cross-thread
            // STW JIT root scan can recover its un-retired reserved tail if it
            // is forcibly stopped mid-JIT. `jvm_thread` is a stack local that
            // is never moved after this point, so the address is stable for
            // the thread's life; cleared just before `mark_dead` below.
            shared_arc
                .threads.thread_registry
                .set_tlab_addr(tid, &jvm_thread.tlab as *const cratonvm_gc::Tlab as usize);
            // XT-FRAME-SCAN: publish the whole `JvmThread` address too (same
            // address-stability argument as the TLAB line above) so a
            // takeover that freezes this worker mid-JIT can walk its
            // interpreter frames for roots newer than its last snapshot
            // deposit. Cleared together with the TLAB address at teardown.
            shared_arc
                .threads.thread_registry
                .set_jvm_thread_addr(tid, &jvm_thread as *const JvmThread as usize);
            // xt-hardening (2026-07-03): publish this worker's OS thread id
            // so the takeover's counted-set excusal can identify it (see
            // ThreadRegistry::set_os_tid_current). Must precede any Java/JIT
            // execution on this thread.
            shared_arc.threads.thread_registry.set_os_tid_current(tid);
            // gci i47w lane oz47 (`vi12-ev.list` T: every row a `[stw-stall]`
            // hang, none with `CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`): bind
            // this OS thread's class-load attribution to its own id, as the
            // attach path (`jni.rs`), the native-spawned threads
            // (`register_native_thread`) and the VM's service threads do. A
            // `Thread.start()` worker never did, so every class it defined
            // was queued with thread id 0 -- `main` -- and a C agent's
            // `ClassLoad` ran on this OS thread inside `in_event_context(0)`
            // with MAIN's `JvmThread` installed and main's `jthread` passed
            // (HotSpot passes the loading thread). A GC-safe JVMTI call in that
            // callback (`RawMonitorEnter` / `Wait`) then opened its blocking
            // region on main -- or, with main already blocked in `join`, none
            // at all -- so this thread waited counted and running, and the
            // collector's pause never got its arrival. Both modes; it changes
            // nothing but the id the class-load hook reports.
            cratonvm_classloading::set_current_thread_id(
                shared_arc.classes.class_layout_domain,
                tid.0,
            );
            jvm_thread.set_vm_state("thread-start:registered");
            loop {
                let marked_ready = shared_arc.mem.gc_barrier.run_if_no_stw_requested(|| {
                    shared_arc.threads.thread_registry.mark_stw_ready(tid);
                });
                if marked_ready {
                    break;
                }

                let pointer_map = shared_arc.mem.gc_barrier.arrive_and_wait_excluded(tid);
                if !pointer_map.is_empty() {
                    crate::runtime::interpreter::apply_pointer_map_to_thread(
                        &mut jvm_thread,
                        &pointer_map,
                        &shared_arc.mem.heap,
                        &shared_arc.classes.type_maps,
                    );
                }
            }
            if shared_arc
                .mem.gc_barrier
                .stw_requested
                .load(std::sync::atomic::Ordering::Acquire)
            {
                crate::runtime::interpreter::safepoint_check(&shared_arc, &mut jvm_thread);
            }
            // WP4.8: For real-JDK virtual threads (e.g.
            // `java.lang.ThreadBuilders$BoundVirtualThread`), `Thread.run()`
            // is overridden вЂ” `BoundVirtualThread.run()` invokes the user's
            // `task.run()`, while the base `Thread.run()` reads `holder.task`
            // (always null for BoundVirtualThread).  We must dispatch
            // virtually on the receiver's actual class id; calling
            // `invoke_shared(... "java/lang/Thread", "run", ...)` would land
            // on the base method and silently do nothing.
            //
            // `invoke_on_class_shared` is the right entry point here вЂ” it's
            // called from invokevirtual/invokespecial and walks the class
            // hierarchy starting at the supplied class.  We pass
            // `class_id_of(thread_obj)` so dispatch starts at the real
            // runtime class (e.g. BoundVirtualThread) and naturally finds
            // the override before falling back to Thread.run().
            let mut run_thread_obj = shared_arc
                .threads.thread_registry
                .java_thread_obj(tid)
                .unwrap_or(thread_obj_for_spawn);
            let recv_cid = shared_arc.mem.heap.class_id_of(run_thread_obj);
            // Gated diagnostic (CRATONVM_DBG_THREADSTART): log each spawned
            // thread's run-class on entry and its result on exit — surfaces
            // threads that never start their target or block inside run().
            let dbg_ts = false;
            if dbg_ts {
                let cn = shared_arc
                    .classes.class_manager
                    .read()
                    .class_store
                    .get(recv_cid)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                eprintln!(
                    "[THREADSTART] tid={} name={:?} run-class={} native-stack={} frame-limit={}",
                    tid.0,
                    name,
                    cn,
                    child_stack_size,
                    jvm_thread
                        .frames
                        .requested_frame_limit()
                        .max(shared_arc.config.max_stack_depth)
                );
            }
            // JDWP `THREAD_START` (interpreter round i4 wave 85, lane L1), on
            // this thread before anything of its own runs (the receiver
            // class's initialization included), as HotSpot posts it; a
            // `SUSPEND_EVENT_THREAD` request parks it at its first bytecode.
            #[cfg(feature = "experimental-debug")]
            crate::debug::send_thread_event_of(
                &shared_arc,
                crate::debug::events::EventKind::ThreadStart,
                tid.0 as u64,
                false,
            );
            jvm_thread.set_vm_state("thread-start:ensure-run-class-init");
            // Class is already loaded (heap entry exists); ensure init runs.
            let _ = super::ensure_class_initialized_shared(&shared_arc, &mut jvm_thread, recv_cid);
            jvm_thread.set_vm_state("thread-start:invoke-run");
            // Interpreter round i1 wave 52, lane L1: a C JVMTI agent's
            // `ThreadStart`, on this thread before its `run()`, as HotSpot
            // posts it. One load while no agent listens.
            //
            // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
            // item 1c): delivered with this thread's JNI binding installed
            // from `jvm_thread` itself (`post_on_own_thread`), so a GC-safe
            // JVMTI function the callback calls reborrows this handle rather
            // than the registry's address of the same thread.
            #[cfg(feature = "experimental-debug")]
            if shared_arc.debug.debugger_gates.native_thread_events_armed() {
                crate::runtime::interpreter::post_on_own_thread(&mut jvm_thread, || {
                    crate::jvmti::native_env::post_thread_event(&shared_arc, tid.0 as u64, true)
                });
            }
            if shared_arc
                .mem.gc_barrier
                .stw_requested
                .load(std::sync::atomic::Ordering::Acquire)
            {
                crate::runtime::interpreter::safepoint_check(&shared_arc, &mut jvm_thread);
            }
            run_thread_obj = shared_arc
                .threads.thread_registry
                .java_thread_obj(tid)
                .unwrap_or(thread_obj_for_spawn);
            #[cfg(feature = "experimental-debug")]
            let result = match agent_body {
                Some(body) => {
                    crate::jvmti::native_env::run_agent_thread_body(&shared_arc, &mut jvm_thread, body);
                    Ok(None)
                }
                None => invoke_on_class_shared(
                    &shared_arc,
                    &mut jvm_thread,
                    recv_cid,
                    "run",
                    "()V",
                    &[Value::Object(Some(run_thread_obj))],
                ),
            };
            #[cfg(not(feature = "experimental-debug"))]
            let result = invoke_on_class_shared(
                &shared_arc,
                &mut jvm_thread,
                recv_cid,
                "run",
                "()V",
                &[Value::Object(Some(run_thread_obj))],
            );
            jvm_thread.set_vm_state("thread-start:run-returned");
            if let Err(MethodCallFailed::InternalError(VmError::ContinuationYield {
                wake_after_nanos,
            })) = &result
            {
                NativeContextImpl {
                    shared: &shared_arc,
                    thread: &mut jvm_thread,
                }
                .deposit_root_snapshot();
                // STW-YIELD-ARRIVE (2026-09-05, `vthread-probe-intermittent-hang`).
                //
                // The deposit above raised `in_blocked_region`, which excludes this
                // continuation from every pause requested from now on. It does NOT
                // retract it from a pause whose census already ran: that census saw the
                // flag DOWN, counted this `tid` in `expected`, and is waiting for it to
                // arrive. Once `suspend_runtime` returns, the carrier goes back to
                // `ForkJoinScheduler::wait_for_task_until` and nothing on this OS thread
                // will ever arrive for `tid` again -- `arrived` cannot reach `expected`,
                // the initiator's `wait_for_all` blocks forever, and the whole VM
                // freezes with no output. That is the `VthreadProbe` hang: bimodal (2
                // runs in 10), independent of machine load, and invisible to every
                // counter because nothing is slow -- one pause is simply waiting for a
                // mutator that has become a heap-resident continuation.
                //
                // The window is the stretch between the last per-bytecode safepoint poll
                // and the deposit: the `Thread.sleep` native plus the unwind that carries
                // `ContinuationYield` out of `execute_frame`. Narrow, which is why the
                // hang is intermittent rather than certain.
                //
                // Fix: take the blocked-region transition under the barrier lock and, if
                // a pause was already in progress, arrive for it exactly once -- the same
                // handshake the TERMINATION path and `NativeContextImpl::park` already
                // perform. `arrive_and_wait_auto` is the required flavour: whether this
                // pause counted us depends on whether its census ran before or after the
                // deposit above, which is not decidable locally (see
                // `GcBarrierInner::excluded_blocked`). The returned `PointerMap` is
                // deliberately dropped -- a thread whose `in_blocked_region` is up is
                // remapped through `fold_pointer_map_into_blocked` into
                // `gc_block_state.fixup`, which the next mount drains in
                // `check_post_block_gc`; applying the map here too would move twice.
                {
                    let blocked = shared_arc.mem.gc_barrier.enter_blocked();
                    if blocked.pre_stw {
                        let _ = shared_arc.mem.gc_barrier.arrive_and_wait_auto(tid);
                    }
                }
                let boxed = Box::new(jvm_thread);
                shared_arc.threads.thread_registry.set_jvm_thread_addr(
                    tid,
                    (&*boxed as *const JvmThread) as usize,
                );
                // gen r5w1/crash5: the TLAB moved with it. `tlab_addr` kept
                // naming the moved-from stack local, which every pause read
                // (`collect_reserved_tlab_tails`) until this OS thread finished.
                shared_arc.threads.thread_registry.set_tlab_addr(
                    tid,
                    &boxed.tlab as *const cratonvm_gc::Tlab as usize,
                );
                shared_arc.threads.virtual_thread_manager.suspend_runtime(
                    tid.0,
                    boxed,
                    std::time::Duration::from_nanos(*wake_after_nanos),
                );
                return;
            }
            if dbg_ts {
                eprintln!(
                    "[THREADEND] tid={} name={:?} result={}",
                    tid.0,
                    name,
                    if result.is_ok() { "ok" } else { "ERR" }
                );
            }
            // Reachability probe for the uncaught-handler path. An `Ok` here
            // when the Runnable definitely threw means the exception was
            // swallowed BELOW this frame and no amount of work in the `Err` arm
            // can matter — see the force-native `java/lang/Thread.run()V` shim
            // in native-builtins, which discards `invoke_virtual`'s Result with
            // `let _ =`.
            if None::<std::ffi::OsString>.is_some() {
                eprintln!(
                    "[dbg-uncaught] tid={} run() returned {} (Err is required for the \
                     UncaughtExceptionHandler path to run at all)",
                    tid.0,
                    if result.is_ok() { "Ok" } else { "Err" },
                );
            }
            // Interpreter round i4 wave 99, lane L6
            // (`CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`, page
            // `i94-L6-thread-death-sweeps-every-inflated-monitor`): did this
            // thread's Java end structurally -- `run()` returned or threw, as
            // did the uncaught-exception dispatch and `Thread.exit()` below? A
            // VM error propagated past their frames instead may leave a
            // monitor owned, and keeps the death's full sweep (the platform
            // twin of wave 98's `ended_structurally` in `run_slice`).
            let mut ended_structurally =
                !matches!(result, Err(MethodCallFailed::InternalError(_)));
            if let Err(e) = result {
                // W1-C: dispatch the per-Thread (or default)
                // UncaughtExceptionHandler before dropping the exception.
                // HotSpot calls Thread.dispatchUncaughtException(Throwable)
                // when Thread.run() escapes; we do the same. The Java method
                // walks the per-instance handler -> ThreadGroup -> default
                // chain itself, so we just need to call it. Any error from
                // the handler dispatch is swallowed (HotSpot does the same:
                // a buggy handler can't take down the VM further than the
                // original exception already did).
                if let MethodCallFailed::ExceptionThrown(exc) = &e {
                    // See the identical fix + rationale at the mirror site in
                    // `runtime::interpreter`'s exception-unwind loop: only
                    // trust `native_pending_return` as a substitute for the
                    // real thrown exception when `exc` itself has gone stale
                    // (GC-relocated during the failing call) — never
                    // unconditionally, or a leftover native-return value
                    // (e.g. an ANTLR `CommonToken`) silently misattributes
                    // the uncaught exception's reported class. Always drain
                    // the slot so a leftover can't poison a later event.
                    let pending_return = jvm_thread.native_pending_return.take();
                    let exc = if shared_arc.mem.heap.is_object_address(exc.as_ptr() as usize).is_some() {
                        *exc
                    } else {
                        pending_return.unwrap_or(*exc)
                    };
                    // GC-root gap: `exc_ref` is a bare Rust local at this
                    // point — `run()`'s frame has already popped (the
                    // exception unwound past it) and
                    // `dispatchUncaughtException`'s frame doesn't exist yet,
                    // so no interpreter frame covers it. `invoke_on_class_shared`
                    // below is re-entrant (it can lazily init classes / allocate
                    // on the way to `ThreadGroup.uncaughtException`'s default
                    // handler chain), so a GC in that window relocates the
                    // Throwable while nothing roots it, corrupting it before
                    // `dispatchUncaughtException`/`printStackTrace` ever see it
                    // (observed as a stale invokevirtual receiver on
                    // `Throwable.printStackTrace`, e.g. an LDAP listener's
                    // "Exception in thread" logging). Pin it in
                    // `native_pin_roots` for the call, matching every other
                    // raw-ObjectRef-across-a-reentrant-call site.
                    let pin_base = jvm_thread.native_pin_roots.len();
                    jvm_thread.native_pin_roots.push(exc);
                    let exc_ref = jvm_thread.native_pin_roots[pin_base];
                    if None::<std::ffi::OsString>.is_some() {
                        let cid = shared_arc.mem.heap.class_id_of(exc_ref);
                        let cname = shared_arc
                            .classes.class_manager
                            .read()
                            .get_class(cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| format!("<unknown class_id={}>", cid.as_u32()));
                        let to_string = invoke_on_class_shared(
                            &shared_arc,
                            &mut jvm_thread,
                            cid,
                            "toString",
                            "()Ljava/lang/String;",
                            &[Value::Object(Some(exc_ref))],
                        )
                        .ok()
                        .flatten()
                        .and_then(|v| match v {
                            Value::Object(Some(s)) => crate::vm::vm_object::read_java_string(&shared_arc.mem.heap, s),
                            _ => None,
                        })
                        .unwrap_or_else(|| "<toString unavailable>".to_string());
                        eprintln!(
                            "[dbg-uncaught] tid={} thread_name={:?} exc_class={} exc={} ptr={:p}",
                            tid.0,
                            name,
                            cname,
                            to_string,
                            exc_ref.as_ptr(),
                        );
                    }
                    // W1-C (real-JDK, 2026-07-27): the handler lookup + invoke
                    // + `dispatchUncaughtException` fallback all live in
                    // `dispatch_uncaught_exception_shared`, which the
                    // virtual-thread death path calls too — see its doc comment
                    // for why consulting the side table here is load-bearing.
                    //
                    // Re-read the pinned Throwable rather than reusing the
                    // `exc_ref` copy taken above: the gated
                    // `CRATONVM_DBG_UNCAUGHT` block between them runs a
                    // re-entrant `toString` invoke that can move it. The helper
                    // pins both refs again for its own re-entrant calls.
                    let exc_now = jvm_thread.native_pin_roots[pin_base];
                    let thread_now = shared_arc
                        .threads.thread_registry
                        .java_thread_obj(tid)
                        .unwrap_or(thread_obj_for_spawn);
                    let dispatch_result = dispatch_uncaught_exception_shared(
                        &shared_arc,
                        &mut jvm_thread,
                        thread_now,
                        exc_now,
                        recv_cid,
                    );
                    // gen r4w3/rooting: the handler dispatch ran Java (a GC
                    // point); the failure report below describes `exc_now`, so
                    // re-read it from the still-live pin before releasing it.
                    let exc_now = jvm_thread.native_pin_roots[pin_base];
                    jvm_thread.native_pin_roots.truncate(pin_base);
                    if matches!(dispatch_result, Err(MethodCallFailed::InternalError(_))) {
                        ended_structurally = false;
                    }
                    if let Err(de) = dispatch_result {
                        // NAME both throwables. This used to print two raw
                        // `ObjectRef { ptr: 0x... }` addresses, which says a
                        // thread died and a handler died with it and nothing
                        // whatsoever about either — the state the Tomcat
                        // `TestNonBlockingAPI` double fault was first reported
                        // in, where neither exception could be identified from
                        // the log at all.
                        //
                        // `describe_throwable` deliberately reads the class and
                        // `detailMessage` out of the heap instead of invoking
                        // `toString()`: the commonest reason the handler
                        // dispatch fails in the first place is that the heap
                        // cannot serve an allocation, and a reporter that needs
                        // three of them would fail for the same reason and
                        // print nothing.
                        //
                        // JIT round 20 wave 8 (lane huntMT): a handler that
                        // THREW is HotSpot's `JavaThread::exit` report, exactly:
                        // a blank line, then `Exception: <binary class name>
                        // thrown from the UncaughtExceptionHandler in thread
                        // "<Thread.name now>"` -- nothing about the original
                        // throwable (the handler had it). This printed an
                        // `Exception in thread` line for the original plus a VM
                        // line, both with internal class names (`java/lang/X`)
                        // and the second with the VM thread id. The main
                        // thread's twin is `dispatch_main_uncaught_exception`.
                        if let MethodCallFailed::ExceptionThrown(d) = &de {
                            let thrown = {
                                let cid = shared_arc.mem.heap.class_id_of(*d);
                                shared_arc
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(cid)
                                    .map(|c| c.name.replace('/', "."))
                                    .unwrap_or_else(|| "java.lang.Throwable".to_string())
                            };
                            // `Thread.getName()` as it reads now (the run may
                            // have renamed the thread), through the registry's
                            // mirror, which collections keep current; the
                            // start-time name when the field cannot be read.
                            let now_name = shared_arc
                                .threads
                                .thread_registry
                                .java_thread_obj(tid)
                                .and_then(|obj| {
                                    let slot = name_slot?;
                                    match shared_arc.mem.heap.get_field(obj, slot) {
                                        Value::Object(Some(s)) => {
                                            crate::vm::vm_object::read_java_string(&shared_arc.mem.heap, s)
                                        }
                                        _ => None,
                                    }
                                })
                                .unwrap_or_else(|| name.clone());
                            eprintln!(
                                "\nException: {thrown} thrown from the UncaughtExceptionHandler \
                                 in thread \"{now_name}\""
                            );
                        } else {
                            // The VM itself failed to run the JDK chain (not a
                            // Java exception): no HotSpot text exists, so name
                            // both failures for the log.
                            let first = shared_arc
                                .mem
                                .heap
                                .is_object_address(exc_now.as_ptr() as usize)
                                .map(|_| describe_throwable(&shared_arc, exc_now))
                                .unwrap_or_else(|| format!("{e:?}"));
                            eprintln!("Exception in thread \"{name}\" {first}");
                            eprintln!(
                                "Thread {tid} terminated with error: {first} \
                                 (dispatchUncaughtException also failed: {de:?})",
                            );
                        }
                    }
                } else {
                    eprintln!("Thread {} terminated with error: {:?}", tid, e);
                }
            }
            // Java-side thread teardown — `Thread.exit()`, which HotSpot's VM
            // calls here and this VM never called at all. See
            // `run_thread_exit_shared` for the JDK body, the measurement, and
            // the full argument for why this sits exactly here.
            //
            // Deliberately OUTSIDE the `if let Err(e) = result` block above, so
            // it runs on the normal and the abnormal path alike. The abnormal
            // one is the one that matters most: a task that ends by throwing is
            // precisely when a structured-concurrency owner is parked waiting
            // for the flock count to fall, and a cleanup that only covers the
            // happy path leaves exactly the leak this fixes.
            //
            // Re-read the `Thread` object from the registry rather than reusing
            // `run_thread_obj` from before the `run()` invoke: the registry copy
            // is remapped after every GC (`update_thread_objs_after_gc`) and the
            // captured one is not, and `run()` plus the uncaught dispatch is an
            // arbitrarily long window for a moving collection — the same stale-ref
            // UAF the `wake_obj` read further down documents.
            let exit_thread_obj = shared_arc
                .threads.thread_registry
                .java_thread_obj(tid)
                .unwrap_or(thread_obj_for_spawn);
            if !run_thread_exit_shared(&shared_arc, &mut jvm_thread, exit_thread_obj) {
                ended_structurally = false;
            }
            // `Thread.exit()` has run; no Java runs on this OS thread again.
            // Drop the JNI global roots behind its `ThreadLocal` values (the
            // intrinsics keep them in a Rust `thread_local!` whose destructor
            // frees the map but not the handles), or every value of every
            // thread that ever died stays a GC root for the life of the VM —
            // see `phases_early::release_current_thread_locals`. Platform
            // threads only: a virtual thread's OS thread is a shared carrier.
            // Before `tlab.retire()` / `mark_dead()`: `current_thread_object`
            // needs this thread's registry entry.
            cratonvm_native_builtins::phases_early::release_current_thread_locals(
                &mut NativeContextImpl {
                    shared: &shared_arc,
                    thread: &mut jvm_thread,
                },
            );
            // The weak handle of an exception reported to a debugger and never
            // caught (the uncaught one that ended `run()`) dies with the thread.
            crate::runtime::interpreter::release_debugger_thread_state(
                &shared_arc,
                &mut jvm_thread,
            );
            // (No carrier-permit release on exit: unreachable here — see the
            // note at the top of this closure — and the permit pool it
            // targeted owned no carrier.)
            // SBR5 lane jfr: jdk.ThreadEnd and the thread's event buffer.
            cratonvm_native_builtins_jfr::jfr_vm_events::thread_exiting(
                &mut NativeContextImpl {
                    shared: &shared_arc,
                    thread: &mut jvm_thread,
                },
                exit_thread_obj,
            );
            // Record JFR thread end event
            {
                let now_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                let mut jfr = shared_arc.debug.flight_recorder.lock();
                // Round-9 HIGH-5: prefer the `_arc` variant.
                let name_arc: std::sync::Arc<str> = std::sync::Arc::from(name.as_str());
                cratonvm_jfr::builtin::emit_thread_end_event_arc(
                    &mut jfr, name_arc, tid.0, now_ns,
                );
            }
            // gci i12/ev (`CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`): the
            // last own-handle point of this thread: deliver the `ClassLoad`s
            // still deferred on it. One load while none waits.
            #[cfg(feature = "experimental-debug")]
            if shared_arc
                .debug
                .deferred_class_loads_pending
                .load(std::sync::atomic::Ordering::Acquire)
            {
                crate::runtime::interpreter::post_on_own_thread(&mut jvm_thread, || {
                    crate::jvmti::native_env::flush_deferred_class_loads(&shared_arc, tid.0 as u64)
                });
            }
            // Fire JVMTI ThreadEnd event
            #[cfg(feature = "experimental-debug")]
            {
                let env = shared_arc.debug.jvmti_env.lock();
                if env.event_manager.is_enabled(crate::jvmti::JvmtiEvent::ThreadEnd) {
                    crate::jvmti::notify_thread_end(&env, tid.0 as u64);
                }
            }
            // Interpreter round i1 wave 52, lane L1: a C JVMTI agent's
            // `ThreadEnd`, on this thread after its `run()`. gci i12/ev (item
            // 1c): on this thread's own handle, as `ThreadStart` above.
            #[cfg(feature = "experimental-debug")]
            if shared_arc.debug.debugger_gates.native_thread_events_armed() {
                crate::runtime::interpreter::post_on_own_thread(&mut jvm_thread, || {
                    crate::jvmti::native_env::post_thread_event(&shared_arc, tid.0 as u64, false)
                });
            }
            // Fire JDWP ThreadDeath event
            //
            // Interpreter round i5 wave 106, lane L1: a `ThreadDeath` whose
            // own event set suspended (`SUSPEND_EVENT_THREAD` / `SUSPEND_ALL`)
            // holds this thread here, GC-blocked, until the resume, before
            // TERMINATED and the joiners' wake-up below, as HotSpot's
            // `JavaThread::exit` (`ensure_join` after `post_thread_end`).
            // Lane L6 did this for `main` in wave 105. Hand-back: ONLY that
            // event's suspension holds; a suspension the thread did not cause
            // (an invocation's closing re-suspension, another thread's) never
            // does (`L1W25JdiStopMonitors` hung when it did).
            #[cfg(feature = "experimental-debug")]
            if crate::debug::send_thread_death_event(&shared_arc, tid.0 as u64) {
                let mut ctx = NativeContextImpl {
                    shared: &shared_arc,
                    thread: &mut jvm_thread,
                };
                // A wait inside the VM (RUNNABLE in `JavaThread::exit`), not a
                // park: no JDK wait kind.
                ctx.begin_vm_wait_region();
                crate::debug::hold_ending_thread_while_suspended(&shared_arc, tid.0 as u64);
                ctx.end_blocking_region();
            }
            // CRIT (terminating-thread TLAB tail): this worker is about to
            // enter termination-monitor blocking and then clear its published
            // TLAB address before `jvm_thread` is dropped. Retire while the
            // thread is still a live, running mutator so the unused tail is
            // filled for any non-moving sweep and no later collection has to
            // parse raw zeroed TLAB bytes with no owner to report a skip region.
            jvm_thread.tlab.retire();
            // CRIT (stale-ref UAF) — `thread_obj_for_spawn` is a raw ObjectRef
            // captured at spawn and NEVER remapped. Over this thread's whole
            // lifetime the GC relocates the Thread object (or a young-GC `grow`
            // frees the arena buffer it last lived in), so by the time we run
            // here the captured ref is stale and the `enter_or_contend` CAS on
            // its header below would write into freed memory →
            // EXCEPTION_ACCESS_VIOLATION (observed: a dying TaskThread worker).
            // The thread registry's `java_thread_obj` IS remapped after every
            // GC (`update_thread_objs_after_gc`), so read the current address
            // from there while the thread is still registered (before
            // `mark_dead`), falling back to the captured ref only if absent.
            let wake_obj = shared_arc
                .threads.thread_registry
                .java_thread_obj(tid)
                .unwrap_or(thread_obj_for_spawn);

            // WP4.1 — wake any thread waiting in `Thread.join()` for us.
            //
            // Acquire a stable inflated monitor handle while this thread is
            // still alive. After `mark_dead`, GC no longer scans this thread's
            // locals, so the final notify/exit must not re-lookup through the
            // raw `Thread` object address; a moving GC may have remapped that
            // object and the monitor-table key. The `Arc<Monitor>` remains
            // valid across such remaps.
            let term_monitor = match shared_arc.threads.monitors.enter_inflated_or_contend(wake_obj, tid) {
                Ok((monitor, contended)) => {
                    if contended {
                        // GCBARRIER-LIVELOCK-FIX (2026-07-18): every other
                        // `block_enter` call site in this codebase (see
                        // `monitor_enter_blocking`, `monitor_enter_synchronized_method`)
                        // deposits a root snapshot — which raises
                        // `gc_block_state.in_blocked_region` — BEFORE calling
                        // `GcBarrier::enter_blocked()`, exactly as
                        // `Monitor::block_enter`'s own doc comment requires
                        // ("The caller MUST have marked itself GC-blocked
                        // first (deposit roots + `GcBarrier::enter_blocked`)").
                        // This call site skipped the deposit: `enter_blocked()`
                        // alone only bumps the barrier's legacy `threads_blocked`
                        // atomic, but the production census
                        // (`request_stw_counted_with_live_blocked` /
                        // `alive_count_blocked_and_os_tids`) excludes a thread
                        // from `expected` by reading `in_blocked_region`, NOT
                        // that atomic. A STW requested AFTER `enter_blocked()`'s
                        // `pre_stw` snapshot but WHILE this thread is still
                        // parked in `monitor.block_enter()` below therefore
                        // counted this (terminating) thread in `expected` —
                        // and `block_enter()` never checks safepoints, so it
                        // could never arrive. If the monitor's current owner
                        // (e.g. a `Thread.join()` caller) was itself waiting
                        // out that same pause before its next native call
                        // could release the monitor (`Object.wait()`'s
                        // release happens only once the native actually
                        // runs), the result was the exact three-way deadlock
                        // `block_enter`'s doc warns about — "owner waits GC,
                        // contender waits owner, GC waits contender" — except
                        // here the GC-blocked prerequisite was never met, so
                        // the barrier's `expected` count permanently included
                        // a slot no thread could ever satisfy: a real,
                        // reproducible livelock under rapid thread churn
                        // (root-caused live via gdb + `CRATONVM_DBG_STW_CENSUS`,
                        // caught as a `blocked=false` alive thread wedged
                        // between `thread-start:run-returned` and the
                        // termination sequence's own later, correct
                        // `deposit_root_snapshot()`).
                        //
                        // Round 12 wave 1 (lane lock): and the rest of
                        // `monitor_enter_blocking`'s protocol, which this site
                        // still skipped. (1) The TLAB is retired BEFORE the
                        // deposit: a collection during the park may reset the
                        // young arena it points into, and the `tlab.retire()`
                        // below would then write its filler over other
                        // objects. (2) The wake goes through
                        // `check_post_block_gc`: without it `in_blocked_region`
                        // stayed raised while this thread went on to write that
                        // filler (the heap write a census that excluded it does
                        // not wait for), and the fixup of every collection it
                        // slept through was never applied, so the deposit below
                        // republished pre-move addresses from its off-frame
                        // slots as roots.
                        let mut ctx = NativeContextImpl {
                            shared: &shared_arc,
                            thread: &mut jvm_thread,
                        };
                        ctx.thread.tlab.retire();
                        ctx.deposit_root_snapshot();
                        let blk = shared_arc.mem.gc_barrier.enter_blocked();
                        if blk.pre_stw {
                            // GCAUDIT-0711-FIX (finding 1a): `_auto` for
                            // uniformity with every other barrier arrival —
                            // the deposit above just raised `in_blocked_region`,
                            // so this pause's own census may have already
                            // excluded us; only the exclusion snapshot the
                            // census recorded (not this thread's guess) can
                            // say which.
                            let _ = shared_arc.mem.gc_barrier.arrive_and_wait_auto(tid);
                        }
                        monitor.block_enter(tid);
                        drop(blk);
                        ctx.check_post_block_gc();
                    }
                    Some(monitor)
                }
                Err(e) => {
                    eprintln!(
                        "Thread {} could not acquire termination monitor for join notification: {:?}",
                        tid, e
                    );
                    None
                }
            };

            // SBR round 2 (lane `thread`): HotSpot's `ensure_join`, in the same
            // position -- while the thread OWNS its own `Thread` object's
            // monitor and before the `notify_all` below: `threadStatus` becomes
            // TERMINATED and `eetop` becomes 0, which is what makes the JDK's
            // `Thread.isAlive()` false and lets `Thread.join()`'s
            // `while (isAlive()) wait(0)` loop exit on the notify. A still
            // running mutator here (`mark_dead` is below), so the heap write is
            // safe; the mirror is read back from the registry (remapped on
            // every collection).
            {
                let mirror = shared_arc
                    .threads
                    .thread_registry
                    .java_thread_obj(tid)
                    .unwrap_or(wake_obj);
                set_thread_state_fields(&shared_arc, mirror, Some(0), Some(THREAD_STATUS_TERMINATED));
                // `java_state` 4 = "terminating": the deposit below must not
                // map a stale WAITING/BLOCKED value over the TERMINATED word.
                jvm_thread
                    .gc_block_state
                    .java_state
                    .store(JAVA_STATE_TERMINATING, std::sync::atomic::Ordering::Release);
            }

            // SATB (G1MARK-2): drain this thread's per-thread SATB buffer
            // BEFORE the thread dies. The buffer is `thread_local!`; when the
            // OS thread exits, its TLS destructor drops the only strong Arc
            // and the registry prunes the dead Weak — any unflushed entries
            // (up to 255 overwritten references logged since the last
            // safepoint) vanish. SATB entries are OLD reference values the
            // MARKER needs, not the logging thread: a dying thread that
            // overwrote the last snapshot-visible path to an object would
            // take its only gray-source to the grave, and cleanup could then
            // free a live region. Cheap no-op when no marking cycle is
            // active.
            shared_arc.mem.heap.flush_thread_satb();

            // gci i14/pn: drop the G1 JNI critical-section pins this thread
            // opened and never released (`CRATONVM_JNI_CRITICAL_G1_PIN`; no
            // record exists without it, so this is one empty-map lock).
            let _ = crate::native::jni::release_critical_pins_opened_by(&shared_arc, tid);

            // CRIT (multi-thread STW deadlock / undercount): transition from
            // alive mutator to dead thread through the blocked-region protocol.
            // Publish a complete root snapshot and the registry blocked flag
            // before entering the terminal barrier transition. A GC request can
            // otherwise land after `enter_blocked()` but before `mark_dead()`:
            // the live-thread census counts this worker while `finish_after()`
            // waits out that pause, leaving one permanently outstanding arrival.
            // The snapshot makes the worker identity-excluded for every such
            // request and lets the collector maintain its terminal roots.
            //
            // Retire this worker's TLAB FIRST, exactly as the main-thread
            // teardown already does before its own `clear_tlab_addr`. A TLAB
            // carve advances the owning region's `cursor` over the WHOLE chunk,
            // so the un-allocated tail `[cursor, end)` sits inside the region's
            // walked `[0, cursor)` range while holding no object grid. Two
            // mechanisms normally cover that: `retire()` stamps a walkable
            // filler over the tail, and — for a thread frozen before it could
            // retire — `collect_reserved_tlab_tails` publishes the span so the
            // walkers stride past it. `clear_tlab_addr` below withdraws this
            // thread from the second mechanism, so without the first the tail
            // becomes a hole owned by nobody: a later linear walk of that
            // region reads the zeroed span as 16-byte all-zero objects and
            // strides off the object grid. `retire()` is idempotent, so doing
            // it here is safe even when a safepoint park already retired.
            //
            // SCOPE — this closes a real asymmetry (the main-thread teardown a
            // few thousand lines up already retires before its own
            // `clear_tlab_addr`; this path did not), but it is NOT the fix for
            // the open G1 corruption in
            // `docs/feature-designs/real-jdk-mode.md`.
            // Measured: with this change,
            // `Log4J2LoggingSystemTests -XX:+UseG1GC` still fails 18/61 with
            // 239/252 `g1::get_field ... num_slots=0 class_id=ClassId(0)` hits
            // and 8 walk breaks per run — unchanged from without it. So a dying
            // worker's un-retired TLAB is not what produces the unwalkable
            // span that page is chasing; that producer is still unidentified.
            jvm_thread.tlab.retire();
            NativeContextImpl {
                shared: &shared_arc,
                thread: &mut jvm_thread,
            }
            .deposit_root_snapshot();
            // Wave 99 (lane L6): whether the sweep below may skip its walk,
            // read while the registry entry (its JNI `MonitorEnter` count) is
            // alive.
            let skip_death_sweep = crate::vm::jdk_continuation::platform_death_may_skip_sweep(
                &shared_arc,
                &jvm_thread,
                ended_structurally,
            );
            // `finish_after` serializes `mark_dead` with `request_stw_counted`,
            // so a GC initiator cannot observe this thread as both dead and
            // still included in `threads_blocked`.
            let term_blk = shared_arc.mem.gc_barrier.enter_blocked();
            if term_blk.pre_stw {
                // GCAUDIT-0711-FIX (finding 1a): `_auto` for uniformity —
                // see the identical note a few lines up.
                let _ = shared_arc.mem.gc_barrier.arrive_and_wait_auto(tid);
            }
            term_blk.finish_after(|| {
                // BUG-03 — stop publishing this worker's TLAB address before
                // the `JvmThread` (and its TLAB) is dropped at closure end, so
                // the collector can never read a dangling pointer.
                shared_arc.threads.thread_registry.clear_tlab_addr(tid);
                shared_arc.threads.thread_registry.mark_dead(tid);
            });
            // A thread that terminated while blocked inside a native call
            // made from within a `synchronized` region never executes its
            // `monitorexit` bytecode — sweep anything it still holds so no
            // future locker waits forever (see
            // `MonitorTable::release_monitors_held_by`). Exclude
            // `term_monitor`: this thread deliberately still owns it here
            // so the notify below can wake `Thread.join()` waiters — the
            // blanket sweep must not force-release it first, or the
            // notify silently no-ops as `NotOwner` (lost-wakeup bug, see
            // `release_monitors_held_by_except`'s doc comment).
            //
            // After `mark_dead` still (a contender that inflates this
            // thread's thin lock from here on sees a dead owner and
            // releases it itself, `release_dead_thin_owner`), but no longer
            // inside `finish_after` (round 12 wave 1, lane lock): that
            // closure holds the GC barrier's transition lock, which every
            // blocked-region enter and leave in the VM -- every contended
            // monitor park and wake -- queues on, and this is a walk of EVERY
            // inflated monitor (a moving collector never prunes a dead
            // object's entry, so on thread-churn workloads the index holds
            // every `Thread` mirror ever joined). The walk is safe against a
            // concurrent pause's index re-key on its own (`remap_seq`).
            //
            // Interpreter round i4 wave 99, lane L6
            // (`CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`): as HotSpot's
            // `JavaThread::exit`, the walk only for a thread that may still
            // own a monitor -- one it entered outside structured locking (JNI
            // `MonitorEnter`, a VM native's `monitor_enter`) or one a VM error
            // left behind; the thin-lock slot goes back either way.
            crate::vm::jdk_continuation::platform_death_releases_monitors(
                &shared_arc,
                tid,
                term_monitor.as_ref(),
                skip_death_sweep,
            );

            if let Some(monitor) = term_monitor {
                // Wave 89 (lane L6): unmounted virtual threads in `join()` too.
                shared_arc
                    .threads
                    .monitors
                    .vthread_notify_all_waiters_of(&monitor);
                let _ = monitor.notify_all(tid);
                let _ = monitor.exit(tid);
                shared_arc
                    .threads
                    .monitors
                    .vthread_hand_off_after_release(std::sync::Arc::as_ptr(&monitor) as usize);
            }
        };

        let started_now = self.shared.deferred_starts.start_or_defer_with_vm(
            &spawn_vm,
            &thread_name_for_builder,
            move |shared| {
                let handle = java_thread_builder.spawn(java_thread_body).expect(
                    "failed to spawn child Java thread (OS refused; check ulimit / thread count)",
                );
                shared.threads.thread_registry.set_join_handle(tid, handle);
            },
        );
        if started_now && !is_executor_worker {
            // Preserve HotSpot-like parent scheduling by default: Thread.start()
            // should not sleep the submitting thread. The env knob remains for
            // diagnosing legacy starvation cases, but a non-zero default lets
            // tiny async tasks complete before callers can close/cancel them.
            // Do not apply this to real ThreadPoolExecutor workers: CompletableFuture
            // concurrency-limit tests depend on their first tasks entering within
            // a 10 ms window.
            let grace = thread_start_handoff_grace();
            if grace.is_zero() {
                std::thread::yield_now();
            } else {
                std::thread::sleep(grace);
            }
        }
        Ok(None)
    }

    fn thread_join(&mut self, thread_obj: ObjectRef) -> MethodCallResult {
        let Some(tid) = resolve_thread_id_from_thread_obj(self.shared, thread_obj) else {
            return Ok(None);
        };
        // T19.H1 — mark GC-blocked across the join so a stop-the-world
        // GC excludes this thread from `wait_for_all` (it is parked in
        // `JoinHandle::join` and cannot reach an interpreter safepoint).
        // GCAUDIT-0711-FIX (finding 1a, adjacent): retire BEFORE deposit —
        // see `monitor_enter_blocking`. CRIT (TLAB UAF): a STW GC can
        // grow/realloc the young arena while we are joined.
        self.thread.tlab.retire();
        self.thread
            .gc_block_state
            .java_state
            .store(1, std::sync::atomic::Ordering::Release);
        // Deposit root snapshot before blocking so GC can scan this thread.
        // Interpreter round i4 wave 99, lane L1: with the quiet leave's key.
        let leave_terms = self.blocking_leave_terms(1);
        self.deposit_root_snapshot();
        self.record_blocking_leave_key(leave_terms);
        {
            let blk = self.shared.mem.gc_barrier.enter_blocked();
            if blk.pre_stw {
                // GCAUDIT-0711-FIX (finding 1a): `_auto` — the deposit above
                // already raised `in_blocked_region` before this check.
                let _ = self
                    .shared
                    .mem
                    .gc_barrier
                    .arrive_and_wait_auto(self.thread.thread_id);
            }
            self.shared.threads.thread_registry.join(tid);
            drop(blk);
        }
        // Check if GC happened while we were blocked (wave 99: quietly).
        let _kept = self.check_post_block_gc_quiet_refs(&mut []);
        Ok(None)
    }

    fn thread_is_alive(&self, thread_obj: ObjectRef) -> bool {
        // Wave 101 (lane L6): the main thread whose Java side ended
        // (`end_main_thread_java_side`) is not alive to Java.
        // Wave 102: except its `DestroyJavaVM` mirror (`list_destroy_java_vm_thread`).
        let registry = &self.shared.threads.thread_registry;
        resolve_thread_id_from_thread_obj(self.shared, thread_obj)
            .map(|id| registry.is_alive(id) && !registry.java_side_ended_for(id, thread_obj))
            .unwrap_or(false)
    }

    fn thread_registry_id(&self, thread_obj: ObjectRef) -> Option<u64> {
        resolve_thread_id_from_thread_obj(self.shared, thread_obj).map(|id| id.0)
    }

    fn thread_registry_id_is_alive(&self, id: u64) -> bool {
        let registry = &self.shared.threads.thread_registry;
        let id = crate::threading::ThreadId(id);
        registry.is_alive(id) && !registry.java_side_ended(id)
    }

    fn thread_run_state(&self, thread_obj: ObjectRef) -> u8 {
        // A JDK-driven `java.lang.VirtualThread` (`super::jdk_continuation`):
        // its state is the JDK's own `state` field, mapped as
        // `VirtualThread.threadState()` maps it (a RUNNING one answers its
        // carrier's state), not its continuation thread's registry row.
        if let Some(state) = super::jdk_continuation::virtual_thread_run_state(self, thread_obj) {
            return state;
        }
        // Resolve the thread's registry id (synthetic stores it at field 2;
        // real-JDK threads are looked up by object identity). The registry
        // RETAINS dead threads' entries (mark_dead only flips `alive`), so a
        // present-but-not-alive entry is TERMINATED, while a missing entry is a
        // thread that was never started (NEW).
        let tid = resolve_thread_id_from_thread_obj(self.shared, thread_obj);
        match tid {
            None => 0, // NEW — never started
            Some(id) => {
                // Wave 101 (lane L6): `end_main_thread_java_side` -- TERMINATED.
                // Wave 102: its `DestroyJavaVM` mirror is RUNNABLE, as
                // HotSpot's (it waits in the VM, its status never published).
                let registry = &self.shared.threads.thread_registry;
                if registry.is_alive(id)
                    && registry.java_side_ended(id)
                    && !registry.java_side_ended_for(id, thread_obj)
                {
                    1
                } else if self.shared.threads.thread_registry.is_alive(id)
                    && !self.shared.threads.thread_registry.java_side_ended(id)
                {
                    match self.shared.threads.thread_registry.java_block_state(id) {
                        1 => 3, // WAITING
                        2 => 4, // BLOCKED
                        3 => 5, // TIMED_WAITING
                        _ => 1, // RUNNABLE
                    }
                } else {
                    2 // TERMINATED
                }
            }
        }
    }

    fn thread_stack_trace(&self, thread_obj: ObjectRef) -> Vec<StackTraceEntry> {
        // gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual 2): a
        // single thread's `Thread.getStackTrace` is a handshake on HotSpot 25 -- its
        // frame-trace pause prints no `safepoint` line and adds no safepoint count.
        let _handshake = self.shared.mem.gc_barrier.frame_trace_op_scope(None);
        thread_stack_trace_served(self, thread_obj, false).0
    }

    // JIT round 19 wave 11, lane gc11 (`CRATONVM_JMX_THREAD_INFO_HIDDEN_RUN_FRAMES`):
    // a thread dump's stack lists the hidden `Thread.run` frames, as
    // `ThreadInfo`'s does (`add_thread_info_hidden_run_entries`).
    fn thread_dump_stack_trace(&self, thread_obj: ObjectRef) -> Vec<StackTraceEntry> {
        let (mut trace, captured, mut inserted, _) =
            thread_stack_trace_served(self, thread_obj, true);
        add_thread_info_hidden_run_entries(
            self.shared,
            thread_obj,
            &mut trace,
            captured,
            &mut inserted,
        );
        trace
    }

    fn thread_jmx_snapshot(
        &self,
        thread_obj: ObjectRef,
    ) -> Option<cratonvm_native_api::ThreadJmxSnapshot> {
        self.jmx_thread_snapshot(thread_obj, false)
    }

    // Interpreter round i2 wave 61, lane L6 (`CRATONVM_THREAD_INFO_FRAMELESS`):
    // a `ThreadInfo` door that reports no frames captures none.
    fn thread_jmx_snapshot_frameless(
        &self,
        thread_obj: ObjectRef,
    ) -> Option<cratonvm_native_api::ThreadJmxSnapshot> {
        let frameless = self.shared.threads.thread_registry.thread_info_frameless();
        self.jmx_thread_snapshot(thread_obj, frameless)
    }

    // nio21 wave 3, lane L4 (page
    // `r21-aqs-owner-index-is-a-native-call-per-lock-FIXED-20261008.md`): the
    // dump-time heap walk HotSpot answers `lockedSynchronizers` with.
    fn owned_synchronizer_census(&mut self) -> Option<Vec<(ObjectRef, ObjectRef)>> {
        owned_synchronizer_census_in(self.shared, self.thread)
    }

    // Interpreter round i2 wave 61, lane L6 (`CRATONVM_THREAD_DUMP_ONE_PAUSE`;
    // `docs/feature-designs/real-jdk-mode.md`):
    // one frame-trace pause for the whole dump when a target is running,
    // as HotSpot's `VM_ThreadDump` takes one safepoint.
    fn begin_thread_dump(&mut self, threads: &[ObjectRef]) {
        let registry = &self.shared.threads.thread_registry;
        if !registry.thread_dump_one_pause() {
            return;
        }
        let running = threads.iter().any(|&t| {
            self.thread.java_thread_obj != Some(t)
                && resolve_thread_id_from_thread_obj(self.shared, t)
                    .is_some_and(|tid| registry.is_alive(tid) && !registry.is_blocked(tid))
        });
        if !running {
            return;
        }
        // JIT round 25: each target's block state as the pause began
        // (`GcBlockState::dump_parked`), for the parked leaf frames of the
        // traces it makes them publish and the state they describe.
        let parked: Vec<(u64, u8)> = threads
            .iter()
            .filter_map(|&t| resolve_thread_id_from_thread_obj(self.shared, t))
            .map(|tid| (tid.0, registry.java_block_state(tid)))
            .collect();
        // A pause another STW pre-empted published nothing: each capture
        // then takes its own, as before.
        let published = crate::runtime::interpreter::stw_publish_frame_traces(
            self.shared,
            self.thread.thread_id,
        );
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[THREAD_DUMP_ONE_PAUSE] tid={} targets={} published={published}",
                self.thread.thread_id.0,
                threads.len()
            );
        }
        if published {
            let generation = self
                .shared
                .mem
                .gc_barrier
                .gc_generation
                .load(std::sync::atomic::Ordering::Acquire);
            self.thread
                .gc_block_state
                .dump_traces_gen
                .store(generation, std::sync::atomic::Ordering::Relaxed);
            *self.thread.gc_block_state.dump_parked.lock() = parked;
        }
    }

    fn end_thread_dump(&mut self) {
        self.thread
            .gc_block_state
            .dump_traces_gen
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.thread.gc_block_state.dump_parked.lock().clear();
        self.thread.gc_block_state.dump_held.lock().clear();
    }

    // JIT round 25 W10-7 review
    // (`docs/feature-designs/jit-ir-optimizations.md`):
    // every target's held read (`HeldThreadRead`) in the dump's own pauses,
    // as HotSpot's `VM_ThreadDump` reads all its `ThreadInfo`s at one
    // safepoint. The W9-1 / W11-1 retry takes again only the targets whose
    // read was stale or unshaped, all of them in one more pause, so a dump
    // costs at most `held_read_attempts()` pauses instead of that many per
    // lazily parked target. Under `CRATONVM_THREAD_DUMP_ONE_PAUSE=0`, or when
    // no pause can be had, each snapshot takes its own reads as before.
    fn begin_thread_info_dump(&mut self, threads: &[ObjectRef]) {
        use std::sync::atomic::Ordering;
        let registry = &self.shared.threads.thread_registry;
        if !registry.thread_dump_one_pause() {
            return;
        }
        self.thread.gc_block_state.dump_held.lock().clear();
        let targets: Vec<(ThreadId, ObjectRef)> = threads
            .iter()
            .copied()
            .filter(|&t| {
                self.thread.java_thread_obj != Some(t)
                    && super::jdk_continuation::virtual_thread_run_state(self, t).is_none()
            })
            .filter_map(|t| resolve_thread_id_from_thread_obj(self.shared, t).map(|tid| (tid, t)))
            .collect();
        if targets.is_empty() {
            self.begin_thread_dump(threads);
            return;
        }
        let shared = self.shared;
        let attempts = held_read_attempts();
        let mut reads: Vec<Option<HeldThreadRead>> = targets.iter().map(|_| None).collect();
        let mut pending: Vec<usize> = (0..targets.len()).collect();
        let mut parked_at_last: Option<Vec<(u64, u8)>> = None;
        let mut pauses = 0u32;
        for attempt in 1..=attempts {
            // As `begin_thread_dump`: each target's block state as the pause
            // began, for the parked leaf frames of the traces it publishes.
            let parked: Vec<(u64, u8)> = threads
                .iter()
                .filter_map(|&t| resolve_thread_id_from_thread_obj(shared, t))
                .map(|tid| (tid.0, registry.java_block_state(tid)))
                .collect();
            let seqs: Vec<Option<u64>> =
                pending.iter().map(|&i| registry.trace_seq(targets[i].0)).collect();
            let batch = crate::runtime::interpreter::stw_publish_frame_traces_with(
                shared,
                self.thread.thread_id,
                || {
                    pending
                        .iter()
                        .zip(&seqs)
                        .map(|(&i, &seq)| HeldThreadRead::read(shared, targets[i].0, targets[i].1, seq))
                        .collect::<Vec<_>>()
                },
            );
            // A pause another STW pre-empted read nothing: the targets still
            // pending take their own reads, as before.
            let Some(batch) = batch else {
                break;
            };
            pauses += 1;
            parked_at_last = Some(parked);
            let mut again = Vec::new();
            for (&i, mut read) in pending.iter().zip(batch) {
                let (shaped, lacks_native) = self.held_read_shape(&read, targets[i].1);
                if read.stale || !shaped {
                    if attempt < attempts {
                        again.push(i);
                    } else if attempts > 1 && lacks_native {
                        // W11 residual: as `jmx_thread_snapshot`'s last attempt.
                        read.answer_running();
                    }
                }
                reads[i] = Some(read);
            }
            pending = again;
            if pending.is_empty() {
                break;
            }
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[THREAD_INFO_DUMP] tid={} targets={} pauses={pauses} held={}",
                self.thread.thread_id.0,
                targets.len(),
                reads.iter().filter(|r| r.is_some()).count(),
            );
        }
        let Some(parked) = parked_at_last else {
            return;
        };
        // The dump's marker, as `begin_thread_dump` leaves it: its traces and
        // these reads are current while no later pause has run.
        let generation = shared.mem.gc_barrier.gc_generation.load(Ordering::Acquire);
        self.thread
            .gc_block_state
            .dump_traces_gen
            .store(generation, Ordering::Relaxed);
        *self.thread.gc_block_state.dump_parked.lock() = parked;
        *self.thread.gc_block_state.dump_held.lock() = targets
            .iter()
            .zip(reads)
            .filter_map(|(&(tid, _), read)| {
                read.map(|read| (tid.0, Box::new(read) as Box<dyn std::any::Any + Send>))
            })
            .collect();
    }

    fn current_thread_object(&mut self) -> ObjectRef {
        if let Some(obj) = self.thread.java_thread_obj {
            if crate::runtime::env_cache::dbg_watchref() {
                debug_log_thread_mirror_identity(self.shared, self.thread.thread_id.0, obj);
            }
            return obj;
        }
        match self.build_current_thread_object() {
            Ok(obj) => obj,
            // Native dispatch and Java callers own a local OOM scope. A bare
            // host call has no Result channel and retains its fatal contract.
            Err(oom) if crate::runtime::native_oom::unwind_ok() => {
                crate::runtime::native_oom::raise(oom.length, oom.what)
            }
            #[cfg_attr(
                not(test),
                expect(
                    clippy::panic,
                    reason = "infallible host API retains the typed startup-failure channel"
                )
            )]
            Err(_) => std::panic::panic_any(crate::error::VmInitializationError {
                detail: "java.lang.OutOfMemoryError: Java heap space".into(),
            }),
        }
    }

    fn current_thread_object_if_any(&mut self) -> Option<ObjectRef> {
        // Round 17 wave 2 (lane ffm2): never mints the mirror (no GC point).
        self.thread.java_thread_obj
    }

    fn thread_interrupt(&mut self, thread_obj: ObjectRef) {
        // Interpreter round i3 wave 76, lane L6 (`CRATONVM_INTERRUPT_ONE_LOOKUP`,
        // default on since the wave-76 triage; page `i75-L6-proposal-one-lookup-thread-interrupt-20261108`):
        // the target is resolved once -- the registry id from `Thread.eetop`
        // (as `Unsafe.unpark` under `CRATONVM_UNPARK_EETOP`), the id walk only
        // without a live `eetop` -- and ONE `threads` read raises the flag and
        // hands back the entry's park state and JMX waiting monitor
        // (`ThreadRegistry::raise_interrupt`); the Java field goes through the
        // thread-layout memo. Off: the field by name under the class-manager
        // lock, the id walk, `set_interrupted`, `peek_jmx_waiting_monitor` and
        // `find_park_state_for_thread_obj`, each its own lookup. The same
        // flag, the same state, the same wakes, in the same order.
        //
        // Wave 76 (`CRATONVM_PARK_WAKE_AFTER_UNLOCK`, default off): the flag
        // is raised without a wake and the target's park state is woken ONCE,
        // at the end (`ParkState::interrupt_wake_after_unlock`: the permit and
        // every waiter, the mutex released before the notify), where the
        // default takes the state's mutex twice -- `set_interrupted`'s wake,
        // then the unpark below -- and the second lock meets the mutex handed
        // to the woken waiter.
        let shared = self.shared;
        let registry = &shared.threads.thread_registry;
        let one_lookup = registry.interrupt_one_lookup();
        let wake_after_unlock = registry.park_wake_after_unlock();
        // Interpreter round i4 wave 91, lane L6 (`CRATONVM_INTERRUPT_ONE_WAKE`,
        // default off; page `interpreter-i77-L6-proposal-one-trip-interrupt-wake-DONE-20261127`):
        // as `wake_after_unlock`, the flag is raised without a wake and the
        // state is woken once at the end, but with the notify under the mutex
        // (`ParkState::interrupt_wake_locked`). Moot when `wake_after_unlock`
        // already folds the two trips.
        let one_wake = !wake_after_unlock && registry.interrupt_one_wake();
        // SBR wave 6 (lane `thread`): the Java `interrupted` FIELD is the
        // interrupt status the retired `Thread.isInterrupted()`/`interrupted()`
        // bytecode reads (HotSpot writes it in `JVM_Interrupt`), so every path
        // that raises the VM-side flag raises it too. `is_interrupted(true)`
        // clears both.
        //
        // nio21 wave 5 L1: the field and the registry flag are raised together
        // under the target's interrupt lock (`ThreadRegistry::lock_interrupt`,
        // keyed by the flag `Arc` the target's consumes clear), so a consume
        // (`consume_thread_interrupt`) never interleaves between the two. The
        // field slot (through the thread-layout memo under the wave 76/77
        // switches) and the flag are resolved first: the lock is a leaf. The
        // wakes below therefore never store the flag again
        // (`raise_interrupt(tid, false)`, `wake_raised_interrupt`).
        let tid = if one_lookup {
            thread_id_from_eetop(shared, thread_obj)
                .or_else(|| resolve_thread_id_from_thread_obj(shared, thread_obj))
        } else {
            resolve_thread_id_from_thread_obj(shared, thread_obj)
        };
        raise_thread_interrupt(shared, tid, thread_obj);
        // CRATONVM_DBG_INTERRUPT: name the Java frame that raised an interrupt.
        // A spurious interrupt is invisible at the point it is CONSUMED (the
        // victim only sees a flag), so the only way to attribute one is to
        // record the producer. Kept permanently and env-gated for the same
        // reason CRATONVM_DBG_CCE_BT is.
        // Wave 77 (`CRATONVM_INTERRUPT_FIELD_MEMO`): read once per VM.
        let dbg_interrupt = if registry.interrupt_field_memo() {
            registry.dbg_interrupt()
        } else {
            None::<std::ffi::OsString>.is_some()
        };
        if dbg_interrupt {
            eprintln!(
                "CRATONVM_DBG_INTERRUPT: target_obj=0x{:x} target_tid={:?} by_tid={}",
                thread_obj.as_ptr() as usize,
                tid.map(|t| t.0),
                self.thread.thread_id.0
            );
            for (i, f) in self.thread.frames.iter().enumerate().rev().take(12) {
                let cn = self
                    .shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(f.class_id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                eprintln!("  INT-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
            }
        }
        let mut raised_state: Option<std::sync::Arc<crate::threading::ParkState>> = None;
        if let Some(tid) = tid {
            // The interrupted flag was set above, through the registry's
            // `Arc` (cross-thread safe), under the interrupt lock.
            //
            // Wake a target parked in `Object.wait()`, for the same reason the
            // `LockSupport.park` unpark below exists: `interrupt()` only sets a
            // flag, and `Monitor::wait` can observe it no sooner than its next
            // 5 ms poll slice. `Object.wait()` was the one blocking primitive
            // left without a prompt wake — a thread interrupted while waiting
            // sat in the condvar for up to a full slice before throwing
            // `InterruptedException`, and an untimed wait had nothing but that
            // poll to end it.
            //
            // The registry already records which monitor a thread is parked on
            // (written for JMX right before the park, taken right after), so the
            // target is identified without a new side table. The wake consumes
            // no pending notification, and a `Some` that has already gone stale
            // costs one spurious wakeup, which `Object.wait()` permits.
            let waiting_monitor = if one_lookup || wake_after_unlock || one_wake {
                // One read: the flag, the state, the monitor. The flag wake
                // `set_interrupted` gives is given here unless the one wake
                // at the end (`wake_after_unlock`, `one_wake`) replaces it.
                match registry.raise_interrupt(tid, false) {
                    Some((park_state, monitor)) => {
                        if !wake_after_unlock
                            && !one_wake
                            && (registry.park_no_poll() || registry.sleep_no_poll())
                        {
                            park_state.wake_for_interrupt();
                        }
                        raised_state = Some(park_state);
                        monitor
                    }
                    None => None,
                }
            } else {
                // The flag is raised (above, under the interrupt lock); only
                // `set_interrupted`'s wake is left to give.
                registry.wake_raised_interrupt(tid);
                registry.peek_jmx_waiting_monitor(tid)
            };
            if let Some(monitor_obj) = waiting_monitor {
                shared
                    .threads
                    .monitors
                    .wake_waiter_for_interrupt(monitor_obj, tid);
            }
            // A carrier hosting a JDK continuation (`super::jdk_continuation`)
            // shares its interrupt flag and `ParkState` with the continuation's
            // thread, which is the one in `Object.wait` if any: HotSpot's
            // `JavaThread::interrupt` of the carrier wakes the virtual thread's
            // wait (`VirtualThread.interrupt` -> `carrier.setInterrupt()`).
            let vtm = &shared.threads.virtual_thread_manager;
            if vtm.jdk_continuations() {
                if let Some(hosted) = vtm.continuation_runners().hosted_by(tid) {
                    if let Some(monitor_obj) = registry.peek_jmx_waiting_monitor(hosted) {
                        shared
                            .threads
                            .monitors
                            .wake_waiter_for_interrupt(monitor_obj, hosted);
                    }
                }
            }
            // An unmounted virtual thread (parked, sleeping, waiting on a
            // latch) is woken by resubmitting its continuation; the flag set
            // above is what its remount reads (interpreter round i1 wave 24,
            // lane L7; `VirtualThreadManager::interrupt_virtual`). With the
            // one-lookup switch the manager is asked only once a virtual
            // thread exists (its answer is `false` before).
            let vtm = &shared.threads.virtual_thread_manager;
            if (!one_lookup || vtm.may_have_virtual_threads()) && vtm.is_virtual(tid.0) {
                vtm.interrupt_virtual(tid.0);
            }
        }
        // Match HotSpot `Thread.interrupt0`: wake the target if it is parked in
        // `LockSupport.park` (e.g. AQS `ConditionObject.await`). Without this the
        // target only notices at the next 5 ms interrupt poll; an explicit unpark
        // makes the wakeup prompt and matches the JVM contract. (`park_interruptible`
        // re-checks the registry flag, so a not-yet-parked target just sees a
        // spurious permit, which `park` is specified to allow.) The state is the
        // registry entry's -- the one `find_park_state_for_thread_obj` resolves --
        // so that lookup is skipped when the entry was found above.
        let park_state = raised_state.or_else(|| shared.find_park_state_for_thread_obj(thread_obj));
        if let Some(park_state) = park_state {
            // `CRATONVM_PARK_WAKE_AFTER_UNLOCK`: the one wake of the state;
            // otherwise the locked unpark, of every waiter with
            // `CRATONVM_PARK_UNTIMED_NO_POLL` (wave 75), as `unpark_park_state`
            // wakes them (`ParkState::interrupt_unpark`, wave 77).
            // `CRATONVM_INTERRUPT_ONE_WAKE` (wave 91): the permit and every
            // waiter in the one locked trip whose flag wake was skipped above.
            if one_wake {
                park_state.interrupt_wake_locked();
            } else {
                park_state.interrupt_unpark(registry.unpark_shape());
            }
        }
    }

    fn take_debugger_stop(&mut self) -> Option<ObjectRef> {
        // Interpreter round i1 wave 25, lane L1: one load while no
        // `ThreadReference.Stop` is pending (`debug::take_stop_for_blocking_call`).
        #[cfg(feature = "experimental-debug")]
        if self.shared.debug.debugger_gates.stop_pending() {
            return crate::debug::take_stop_for_blocking_call(self.shared, self.thread.thread_id.0);
        }
        None
    }

    fn is_interrupted(&self, clear: bool) -> bool {
        if clear {
            // SBR wave 6 (lane `thread`): a consumed interrupt (`sleep`,
            // `wait`, `join`, `park`, `clearInterruptEvent`, ...) also clears
            // the Java `interrupted` field, as HotSpot does when it throws
            // `InterruptedException`. nio21 wave 5 L1: both clears under the
            // per-thread interrupt lock (`consume_thread_interrupt`).
            return consume_thread_interrupt(
                self.shared,
                &self.thread.interrupted,
                self.thread.java_thread_obj,
            );
        }
        self.thread
            .interrupted
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn enter_interrupt_wake_event(&mut self) -> Option<isize> {
        // Interpreter round i6 wave 115, lane L1
        // (`CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT`, read by the caller): the
        // thread's park-state wake event, which every interrupt wake signals
        // for an announced waiter (`ParkState::announce_event_waiter`). Not
        // for a VM-scheduled virtual thread: its interrupt unparks the
        // virtual thread, not this state.
        #[cfg(windows)]
        let event = if vm_scheduled_virtual(self.thread) {
            None
        } else {
            self.thread.park_state.announce_event_waiter()
        };
        #[cfg(not(windows))]
        let event = None;
        event
    }

    fn leave_interrupt_wake_event(&mut self, consumed: bool) {
        #[cfg(windows)]
        self.thread.park_state.withdraw_event_waiter(consumed);
        #[cfg(not(windows))]
        let _ = consumed;
    }

    fn set_served_timed_join_hint(&mut self, timed: bool) {
        // Round 14 wave 6 (lane trace5; TR5-3): consumed by the next
        // `capture_throwable_stack_trace` on this thread.
        self.thread.served_timed_join_hint = Some(timed);
    }

    fn set_served_loader_miss_hint(&mut self, loader_class: Option<ClassId>) {
        // Round 15 wave 2 (lane trace2): consumed by the next
        // `capture_throwable_stack_trace` on this thread
        // (`append_served_loader_miss_frames`).
        self.thread.served_loader_miss_hint = loader_class;
    }

    fn set_mh_wmte_raise_hint(&mut self, raise: Option<cratonvm_native_api::MhWmteRaise>) {
        // JIT round 16 wave 13 (lane wmte): consumed by the next capture of a
        // `WrongMethodTypeException` on this thread
        // (`append_mh_wmte_raise_frames`). `Invokers` names the frames of both
        // raises (`checkExactType`, `checkGenericType`) and may not be loaded
        // under `--compatible`; the capture finds only loaded classes, so load
        // it (not initialize it) first. A refusal path; one read-locked lookup
        // once it is loaded.
        if raise.is_some() {
            let invokers = crate::runtime::stackwalker::MH_WMTE_INVOKERS_CLASS;
            let loaded = self
                .shared
                .classes
                .class_manager
                .read()
                .resolve_fast_path_class_id(invokers)
                .is_some();
            if !loaded {
                let _ = self.shared.load_class_concurrent(invokers);
            }
        }
        self.thread.mh_wmte_raise_hint = raise;
    }

    fn note_virtual_sleep_interrupt(&mut self, during: bool) -> bool {
        // Interpreter round i1 wave 48, lane L1: consumed by the next
        // `capture_throwable_stack_trace` on this thread.
        if !matches!(self.thread.kind, crate::threading::ThreadKind::Virtual)
        {
            return false;
        }
        preload_virtual_thread_class(self.shared);
        self.thread.virtual_sleep_hint = Some(during);
        true
    }

    fn thread_is_interrupted(&self, thread_obj: ObjectRef) -> bool {
        // Fast path: the receiver IS the current thread (the common case for
        // `Thread.currentThread().isInterrupted()` and AQS self-checks).
        if self.thread.java_thread_obj == Some(thread_obj) {
            return self
                .thread
                .interrupted
                .load(std::sync::atomic::Ordering::Acquire);
        }
        // Cross-thread query (e.g. `ThreadPoolExecutor` checking a worker):
        // resolve the target's registry id and read its shared interrupt flag.
        // Unknown / not-yet-registered threads default to false, matching the
        // registry's "missing means absent" convention. Resolution goes
        // through the tid-keyed resolver — a raw pointer walk here can alias
        // a recycled mirror address to a dead thread's entry.
        resolve_thread_id_from_thread_obj(self.shared, thread_obj)
            .and_then(|tid| {
                self.shared
                    .threads
                    .thread_registry
                    .get_interrupted_flag(tid)
            })
            .map(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
            .unwrap_or(false)
    }

    // -- Virtual thread / Loom hooks (NEW-15) --

    fn is_current_virtual(&self) -> bool {
        matches!(self.thread.kind, crate::threading::ThreadKind::Virtual)
    }

    // -- JDK-driven continuations (`super::jdk_continuation`) --

    fn continuation_enter_special(
        &mut self,
        cont: ObjectRef,
        is_continue: bool,
        is_virtual_thread: bool,
    ) -> Result<(), MethodCallFailed> {
        super::jdk_continuation::enter_special(self, cont, is_continue, is_virtual_thread)
    }

    fn continuation_do_yield(&mut self) -> Result<i32, MethodCallFailed> {
        super::jdk_continuation::do_yield(self.shared, self.thread)
    }

    fn continuation_pinned_reason(&self) -> i32 {
        super::jdk_continuation::pinned_reason(self.thread)
    }

    fn vt_monitor_wait_unmount(
        &mut self,
        obj: ObjectRef,
        timeout_ms: Option<u64>,
    ) -> Option<MethodCallResult> {
        let shared = self.shared;
        if !shared.threads.monitors.vthread_monitor_unmount()
            || !vthread_wait_unmount_allowed(self.thread)
        {
            return None;
        }
        // A pending interrupt: the ordinary wait throws at once.
        if self
            .thread
            .interrupted
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let vthread = self.thread.java_thread_obj?;
        let (state_slot, state) = virtual_thread_state(shared, vthread)?;
        if state != VTHREAD_STATE_RUNNING {
            return None;
        }
        let notified_slot = virtual_thread_field_slot(shared, vthread, "notified")?;
        let timeout_slot = virtual_thread_field_slot(shared, vthread, "timeout")?;
        let tid = self.thread.thread_id;
        // HotSpot's `ObjectMonitor::wait` for a virtual thread: `notified`
        // cleared, the timeout (milliseconds, which `afterYield` schedules
        // `waitTimeoutExpired` with) and WAITING / TIMED_WAITING. Written
        // BEFORE the release, while this thread still owns the monitor: a
        // notify (which needs the monitor) can then only come after, and
        // the unblocker's `notified = true` is never overwritten here.
        // `afterYield` moves WAITING to WAIT / TIMED_WAIT, or on to BLOCKED
        // when a notify came first. Nothing allocates: the refs stay current.
        let heap = &shared.mem.heap;
        heap.set_field(vthread, notified_slot, Value::Int(0));
        let new_state = match timeout_ms {
            Some(ms) if ms > 0 => {
                heap.set_field(
                    vthread,
                    timeout_slot,
                    Value::Long(i64::try_from(ms).unwrap_or(i64::MAX)),
                );
                VTHREAD_STATE_TIMED_WAITING
            }
            _ => VTHREAD_STATE_WAITING,
        };
        heap.set_field(vthread, state_slot, Value::Int(new_state));
        // Enrol, release fully, hand an entrant off -- or IMSE, the state put
        // back: the thread runs on and throws.
        if let Err(e) = shared.threads.monitors.vthread_wait_release(obj, tid) {
            heap.set_field(vthread, state_slot, Value::Int(VTHREAD_STATE_RUNNING));
            return Some(Err(e));
        }
        shared.threads.thread_registry.set_jmx_waiting_monitor(tid, obj);
        self.thread.vt_yield = crate::threading::jvm_thread::VtYield::MonitorWait;
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[VTHREAD_MONITOR_UNMOUNT] tid={} wait, unmounting", tid.0);
        }
        Some(Err(MethodCallFailed::InternalError(VmError::ContinuationYield {
            wake_after_nanos: 0,
        })))
    }

    fn set_current_thread_object(&mut self, thread: ObjectRef) {
        super::jdk_continuation::set_current_thread(self, thread);
    }

    fn current_carrier_thread_object(&mut self) -> ObjectRef {
        super::jdk_continuation::current_carrier_thread(self)
    }

    fn post_virtual_thread_transition(&mut self, kind: u8) {
        super::jdk_continuation::post_transition(self, kind);
    }

    fn blocks_jdk_carrier(&self) -> bool {
        self.thread.jdk_continuation.jdk() && self.thread.jdk_continuation.carrier() != 0
    }

    fn vm_pinned_park_compensates(&self) -> bool {
        // Interpreter round i5 wave 101, lane L2 (`CRATONVM_VTHREAD_COMPILED_LEARNED`).
        super::jdk_continuation::vm_pinned_park_compensates(self.shared, &*self.thread)
    }

    fn take_virtual_thread_list_to_unblock(&mut self) -> Option<ObjectRef> {
        let monitors = &self.shared.threads.monitors;
        let ids = if !monitors.vthread_monitor_unmount() {
            // The trait's answer with the switch off -- but for gci i25/vt's
            // orphans (below), which need the unblocker either way.
            self.begin_vm_wait_region();
            std::thread::sleep(std::time::Duration::from_millis(50));
            self.end_blocking_region();
            Vec::new()
        } else {
            // Interpreter round i4 wave 87, lane L6 (`CRATONVM_VTHREAD_MONITOR_UNMOUNT`):
            // the virtual threads releases handed off (and the scan found free),
            // as HotSpot's `JVM_TakeVirtualThreadListToUnblock`. A short wait: the
            // scan is the safety net for releases that skip the hand-off.
            self.begin_vm_wait_region();
            let ids = monitors.take_vthread_unblock_list(std::time::Duration::from_millis(5));
            self.end_blocking_region();
            ids
        };
        // gci i25/vt: virtual threads whose `afterYield()` died before it
        // scheduled them (moved to BLOCKED by
        // `jdk_continuation::resolve_post_yield_window`); `unblock()`
        // submits them with its retry on `OutOfMemoryError`.
        let ids = super::jdk_continuation::with_post_yield_orphans(self.shared, ids);
        if ids.is_empty() {
            return None;
        }
        // Linked through `VirtualThread.next`, each `onWaitingList`, as the
        // JDK's `unblockVirtualThreads` expects. Field writes only: nothing
        // allocates, so the references stay current.
        let shared = self.shared;
        let registry = &shared.threads.thread_registry;
        let mut head: Option<ObjectRef> = None;
        for tid in ids.iter().rev() {
            let Some(vthread) = registry.java_thread_obj(*tid) else {
                continue;
            };
            let class_id = shared.mem.heap.class_id_of(vthread);
            let (next_slot, waiting_slot) = {
                let cm = shared.classes.class_manager.read();
                (
                    resolve_field_index_in_hierarchy(class_id, "next", &cm.class_store),
                    resolve_field_index_in_hierarchy(class_id, "onWaitingList", &cm.class_store),
                )
            };
            let (Some(next_slot), Some(waiting_slot)) = (next_slot, waiting_slot) else {
                continue;
            };
            // Wave 89: a notified `Object.wait`er -- HotSpot's `INotify` sets
            // `notified` and moves WAIT / TIMED_WAIT to BLOCKED, so
            // `unblock()` (BLOCKED -> UNBLOCKED) resubmits it. Still
            // WAITING / TIMED_WAITING (`afterYield` not run yet): `notified`
            // alone, which `afterYield` reads. A timeout or an interrupt that
            // already made it UNBLOCKED: the CAS fails and nothing changes.
            // Wave 98: the notifier already did this at the notify; repeated
            // here for a notify that reached the waiter another way (the
            // thread-termination `notifyAll`), and for a waiter whose
            // `afterYield` had not run yet then.
            if monitors.vthread_wait_notified(*tid) {
                mark_virtual_waiter_notified(shared, vthread);
            }
            shared
                .mem
                .heap
                .set_field(vthread, next_slot, Value::Object(head));
            shared
                .mem
                .heap
                .set_field(vthread, waiting_slot, Value::Int(1));
            head = Some(vthread);
        }
        head
    }

    fn begin_carrier_compensation(&mut self) -> bool {
        super::jdk_continuation::begin_carrier_compensation(self)
    }

    fn end_carrier_compensation(&mut self, attempted: bool) {
        super::jdk_continuation::end_carrier_compensation(self, attempted);
    }

    fn continuation_stack_trace(&self, cont: ObjectRef) -> Option<Vec<StackTraceEntry>> {
        let tid = super::jdk_continuation::continuation_thread(self, cont)?;
        let mirror = self.shared.threads.thread_registry.java_thread_obj(tid)?;
        // t1 lane E2: the VM half of an unmounted virtual thread's
        // `Thread.getStackTrace()` (`VirtualThread.tryGetStackTrace` ->
        // `Continuation.getStackTrace`'s walker), apart from its Java half.
        // Its thread is in its yield deposit, so no pause or handshake runs
        // (`blocked=true`); `blocked=false` would mean the walk asked a
        // continuation the JDK holds SUSPENDED that this VM still runs.
        let clock = crate::runtime::env_cache::frame_trace().then(std::time::Instant::now);
        // The dump's order is the capture's (outermost first).
        let trace = self.thread_dump_stack_trace(mirror);
        if let Some(t0) = clock {
            eprintln!(
                "[CONT_TRACE] tid={} target={} blocked={} us={} len={}",
                self.thread.thread_id.0,
                tid.0,
                self.shared.threads.thread_registry.is_blocked(tid),
                t0.elapsed().as_micros(),
                trace.len()
            );
        }
        Some(trace)
    }

    fn vt_pin_count(&self) -> u32 {
        // A native that re-entered Java beneath the caller pins the
        // continuation as a held monitor does (wave 26, lane L4; see
        // `native_callee_memo::continuation_pinned_by_native`): the sleep and
        // park natives read this to decide whether to block the carrier.
        self.thread
            .pin_count
            .saturating_add(u32::from(vt_pinned_by_native(&*self.thread)))
    }

    fn vt_pin(&mut self, reason: &'static str) {
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual) {
            self.thread.pin_count = self.thread.pin_count.saturating_add(1);
            self.thread.pin_reason = reason;
        }
    }

    fn vt_unpin(&mut self) {
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual)
            && self.thread.pin_count > 0
        {
            self.thread.pin_count -= 1;
            if self.thread.pin_count == 0 {
                self.thread.pin_reason = "";
            }
        }
    }

    // `vt_release_carrier` / `vt_acquire_carrier` are deliberately NOT
    // overridden — the `NativeContext` defaults (no-ops, see
    // `native-api/src/registry.rs`) are the honest implementation.
    //
    // They used to release/acquire a `virtual_scheduler` permit. Two reasons
    // that was wrong, both verified 2026-07-26:
    //
    //  1. Every caller is unreachable. All six call sites
    //     (`native-builtins/src/lang_system.rs`, the three `Thread.sleep`
    //     natives) are shaped as
    //         let release = is_virtual && !pinned;              // pin_count == 0
    //         if release && ctx.vt_park_for(d) { return ContinuationYield }
    //         if release { ctx.vt_release_carrier(); }
    //     and `vt_park_for` returns exactly `is_virtual && pin_count == 0` —
    //     the same predicate as `release`, with nothing in between that can
    //     change it. So whenever `release` holds the guarded return always
    //     fires first and the two carrier calls are dead code.
    //  2. The permit pool is disjoint from the carriers. Releasing freed no
    //     carrier, and the paired `acquire()` could block a real carrier OS
    //     thread on a permit nobody was going to return.
    //
    // Overriding them with no-ops explicitly (rather than deleting the
    // overrides) would be equivalent; the defaults already say it.

    fn vt_park_for(&mut self, _duration: std::time::Duration) -> bool {
        let yields = vm_scheduled_virtual(self.thread)
            && self.thread.pin_count == 0
            && !vt_pinned_by_native(&*self.thread);
        if yields {
            // Wave 24 (L7): what the remount does with an interrupt
            // (`interpreter::resume_continuation`): a park resumes normally.
            self.thread.vt_yield = crate::threading::jvm_thread::VtYield::Park;
        }
        yields
    }

    fn vt_sleep_for(&mut self, duration: std::time::Duration) -> bool {
        let yields = vm_scheduled_virtual(self.thread)
            && self.thread.pin_count == 0
            && !vt_pinned_by_native(&*self.thread);
        if yields {
            // Wave 24 (L7): a sleep's remount throws on an interrupt and
            // sleeps out the rest of an early, uninterrupted wake.
            self.thread.vt_yield = crate::threading::jvm_thread::VtYield::Sleep {
                deadline: std::time::Instant::now().checked_add(duration),
            };
        }
        yields
    }

    fn vt_wait_on_key(&mut self, key: u64) -> bool {
        if !vm_scheduled_virtual(self.thread)
            || self.thread.pin_count != 0
            || vt_pinned_by_native(&*self.thread)
        {
            return false;
        }
        self.shared
            .threads
            .virtual_thread_manager
            .wait_on_key(key, self.thread.thread_id.0);
        self.thread.vt_yield = crate::threading::jvm_thread::VtYield::KeyedWait { key };
        true
    }

    fn vt_cancel_wait_on_key(&mut self, key: u64) {
        self.shared
            .threads
            .virtual_thread_manager
            .cancel_wait_on_key(key, self.thread.thread_id.0);
    }

    fn vt_wake_waiters(&mut self, key: u64) {
        self.shared.threads.virtual_thread_manager.wake_waiters(key);
    }

    fn continuation_frame_mark(&self) -> usize {
        self.thread.frames.len()
    }

    /// The method-handle doors' half of the lambda door's discipline
    /// (interpreter round i1 wave 28, lane L4): the conversion is applied by
    /// the value-return arms to the remounted frame's return
    /// (`interpreter::note_continuation_return_boundary`, which records
    /// nothing when no conversion is owed or no frame was pushed).
    fn note_continuation_return(&mut self, frame_index: usize, caller_return: &str) {
        let call_descriptor = format!("(){caller_return}");
        crate::runtime::interpreter::note_continuation_return_boundary(
            self.thread,
            frame_index,
            &call_descriptor,
        );
    }

    /// Interpreter round i6 wave 115, lane L4 (`CRATONVM_MH_VOID_TAIL_UNMOUNT`):
    /// a `void` leaf's door `null`. The leaf's Java frame at `frame_index`
    /// gets an adapter (its `void` return pushes the `null`,
    /// `interpreter::push_void_return_across_continuation_boundary`); when no
    /// frame was pushed (a native leaf yielded itself) the caller -- the top
    /// frame, standing past its invoke with its arguments popped -- receives
    /// the `null` now, which the remount (`RemountAction::Run`) resumes over
    /// and a remount throw discards with the operand stack.
    fn note_continuation_void_leaf_null(&mut self, frame_index: usize) {
        let frames = self.thread.frames.len();
        if frames > frame_index {
            crate::runtime::interpreter::note_continuation_return_boundary(
                self.thread,
                frame_index,
                "()Ljava/lang/Object;",
            );
        } else if frames == frame_index && frame_index > 0 {
            let caller = &mut self.thread.frames[frame_index - 1];
            caller.exec_epoch = caller.exec_epoch.wrapping_add(1);
            let _ = caller.stack.push(Value::Object(None));
        }
    }

    fn active_thread_count(&self) -> i32 {
        self.shared.threads.thread_registry.alive_count() as i32
    }

    // Interpreter round i5 wave 100, lane L6: the JMM's platform-thread
    // counters (`ThreadRegistry::note_jmm_thread_start`).
    fn jmm_thread_counters(&self) -> Option<(i64, i32, i32)> {
        Some(self.shared.threads.thread_registry.jmm_counters())
    }

    fn jmm_peak_threads_reset(&self) -> bool {
        self.shared.threads.thread_registry.jmm_peak_reset();
        true
    }

    /// Record an AQS synchronizer's exclusive owner, for
    /// `ThreadInfo.getLockedSynchronizers()`.
    ///
    /// `native-builtins`' `setExclusiveOwnerThread` native already called
    /// this on every AQS ownership change, but only the trait's EMPTY DEFAULT
    /// existed — so the call did nothing and `jmx_locked_synchronizers` was
    /// permanently empty. That is half of why `isSynchronizerUsageSupported()`
    /// had to answer false.
    ///
    /// `owner: None` means the synchronizer was released; the registry writer
    /// clears it from every thread before assigning, so a hand-off between
    /// threads cannot leave it recorded against both.
    fn record_jmx_owned_synchronizer(&mut self, synchronizer: ObjectRef, owner: Option<ObjectRef>) {
        // `CRATONVM_JMX_OWNED_SYNCHRONIZERS=off` — DIAGNOSTIC ONLY, default on.
        //
        // This bookkeeping is two `parking_lot` mutexes, an `FxHashMap`
        // insert/remove and a `Vec` push/retain, and it runs **4.77 times per
        // expired task** on `HashedWheelTimerTest#testExecutionOnTime` (the
        // JIT-arm census, once `setExclusiveOwnerThread` became site-cacheable
        // and therefore countable). Whether that is 30 ns of the native's
        // ~211 ns or 100 of it decides whether it is worth redesigning, and no
        // amount of reading the code answers it — so the switch exists to
        // price it on ONE binary, which is the only A/B that means anything
        // here.
        //
        // It is NOT a supported configuration: turning it off empties
        // `ThreadInfo.getLockedSynchronizers()` and blinds
        // `findDeadlockedThreads()` to every AQS-derived lock, which is
        // exactly the capability `isSynchronizerUsageSupported()` promises.
        if !jmx_owned_synchronizer_index_enabled() {
            return;
        }
        // `CRATONVM_OWNER_RECORD_NEEDS_INDEX` (interpreter round i5 wave 101,
        // lane L6; `VmConfig::owner_record_needs_index`, already false when
        // `CRATONVM_AOS_OWNER_INDEX` is on): with the per-lock index retired,
        // every door that reports `lockedSynchronizers` takes the dump-time
        // heap walk instead, so this record is unread -- and, written only by
        // the `ReentrantLock` fast path, it was also partial (a release made by
        // the JDK body left the entry behind as a GC root). Off (the default),
        // one field load.
        if self.shared.config.owner_record_needs_index {
            return;
        }
        // AQS sets the owner from the owning thread itself — `acquire` passes
        // `Thread.currentThread()`, `release` passes null — so the mirror handed
        // in is virtually always this thread's own. Recognising that by pointer
        // skips `resolve_thread_id_from_thread_obj`, which reads the mirror's
        // `tid` field and then takes the registry's `java_tid_to_id` mutex, on
        // a path that runs twice per uncontended `ReentrantLock` pair.
        //
        // `java_thread_obj` is this thread's mirror and is GC-remapped in step
        // with any `ObjectRef` reaching a native, so the pointer compare is
        // exact rather than heuristic; anything that does not match falls
        // through to the full resolve, which is also what a genuinely
        // cross-thread caller needs.
        let owner_tid = match owner {
            None => None,
            Some(o) if self.thread.java_thread_obj == Some(o) => Some(self.thread.thread_id),
            Some(o) => resolve_thread_id_from_thread_obj(self.shared, o),
        };
        let registry = &self.shared.threads.thread_registry;
        // Both transitions AQS performs — `acquire` passing this thread, and
        // `release` passing null — change only this thread's own list, and this
        // thread can hold a handle to it. Fetched once, then reused, so the
        // registry `RwLock` is off the ownership path entirely. See
        // `ThreadRegistry::set_jmx_owned_synchronizer_own`.
        if owner_tid.is_none() || owner_tid == Some(self.thread.thread_id) {
            if self.thread.jmx_locked_synchronizers.is_none() {
                self.thread.jmx_locked_synchronizers =
                    registry.jmx_locked_synchronizers_of(self.thread.thread_id);
            }
            if let Some(own_list) = self.thread.jmx_locked_synchronizers.as_deref() {
                registry.set_jmx_owned_synchronizer_own(
                    self.thread.thread_id,
                    own_list,
                    owner_tid,
                    synchronizer,
                );
                return;
            }
        }
        registry.set_jmx_owned_synchronizer(owner_tid, synchronizer);
    }

    /// OS thread id behind a Java `Thread` mirror, for arbitrary-thread CPU
    /// time. Same two-step resolve `thread_jmx_snapshot` performs.
    fn thread_os_tid(&self, thread_obj: ObjectRef) -> Option<u32> {
        let tid = resolve_thread_id_from_thread_obj(self.shared, thread_obj)?;
        self.shared.threads.thread_registry.os_tid_of(tid)
    }

    /// Total JIT compile time, already in the JMM's own unit (milliseconds).
    /// zt t18/pl8: the whole milliseconds of the nanosecond total, as
    /// HotSpot's bean converts its `java.ci.totalTime` ticks
    /// (`Management::ticks_to_ms`); the sum of each compile's whole
    /// milliseconds read low, and below the perf counter's figure.
    fn jit_total_compile_time_ms(&self) -> Option<u64> {
        Some(
            self.shared
                .jit
                .tiered_manager
                .stats()
                .total_compile_time_ns
                .load(std::sync::atomic::Ordering::Relaxed)
                / 1_000_000,
        )
    }

    /// gci i44/sp: the GC barrier's safepoint accounting
    /// (`GcBarrier::safepoint_stats`), in the JMM's milliseconds.
    /// `CRATONVM_JMX_SAFEPOINT_COUNTERS=0` restores the constant 0.
    fn safepoint_stats_ms(&self) -> Option<(u64, u64, u64, u64)> {
        if !true {
            return None;
        }
        let s = self.shared.mem.gc_barrier.safepoint_stats();
        Some((
            s.count,
            s.stopped_ns / 1_000_000,
            s.sync_ns / 1_000_000,
            s.application_ns / 1_000_000,
        ))
    }

    /// `(pool_size, mounted, queued)` sampled in ONE call — see the trait doc
    /// for why the three are not separate accessors.
    fn vt_scheduler_stats(&self) -> Option<(i32, i32, i64)> {
        let (pool, mounted, queued) = self
            .shared
            .threads
            .virtual_thread_manager
            .scheduler_counters();
        Some((
            i32::try_from(pool).unwrap_or(i32::MAX),
            i32::try_from(mounted).unwrap_or(i32::MAX),
            i64::try_from(queued).unwrap_or(i64::MAX),
        ))
    }

    /// Real finalization backlog, for `getObjectPendingFinalizationCount()`.
    /// `try_lock` rather than `lock` on the processor: this is a monitoring
    /// read, and blocking a JMX query behind an in-progress GC reference pass
    /// would be a worse failure than briefly under-reporting.
    ///
    /// gc-common w1-d (2026-09-23): the backlog lives in the FINALIZER THREAD's
    /// queue, not the processor's. `ReferenceProcessor::process_references`
    /// drains its own finalization queue into `to_finalize` in the same call
    /// that fills it, and every driver hands that straight to
    /// `FinalizerThread`, so the processor's count is zero at every point a
    /// mutator can observe it — this reported 0 on every VM, always. HotSpot's
    /// number is the length of the queue `finalize()` calls are waiting in,
    /// which is `finalizer_thread.pending_count()`; the processor term is kept
    /// (normally 0) so nothing in flight is missed. The finalizer queue's own
    /// lock is only ever held for a push/pop/remap.
    fn pending_finalization_count(&self) -> i32 {
        let in_processor = match self.shared.mem.ref_processor.try_lock() {
            Some(rp) => rp.pending_finalization_count(),
            None => 0,
        };
        let queued = self.shared.mem.finalizer_thread.pending_count();
        queued.saturating_add(in_processor).min(i32::MAX as usize) as i32
    }

    fn enumerate_threads(&self, max: usize) -> Vec<ObjectRef> {
        let registry = &self.shared.threads.thread_registry;
        // Interpreter round i6 wave 133, lane L7 (`CRATONVM_THREADS_ATTACH_ORDER`,
        // HotSpot parity, default on, `=0` off; i102-L1 item 5): HotSpot walks
        // its threads list, where each thread is appended as it attaches, so
        // `Thread.getThreads()` / `ThreadImpl.getThreads()` / `dumpAllThreads`
        // answer in attach order; the map's hash order differed
        // (`L7W133ThreadListOrder.java`).
        // Wave 101 (lane L6): HotSpot's detached main thread is no longer
        // listed (`end_main_thread_java_side`). One load, and a retain only
        // once the launcher ended it.
        // Wave 102: unless the entry now carries its `DestroyJavaVM` mirror,
        // which HotSpot lists (`list_destroy_java_vm_thread`).
        // Interpreter round i7 wave 136, lane L1b: the ended main thread is
        // dropped BEFORE the list is cut to `max` (it was cut first, so a
        // bounded caller whose cut kept the ended main got `max - 1`
        // entries while a live thread was left out); the unordered walk
        // asks for one more entry when there is one to drop.
        let ended_main = if registry.java_side_ended(crate::threading::ThreadId(0))
            && !registry.destroy_java_vm_listed()
        {
            registry.java_thread_obj(crate::threading::ThreadId(0))
        } else {
            None
        };
        let mut threads = if registry.threads_attach_order() {
            registry.alive_thread_objects_in_id_order()
        } else {
            registry.alive_thread_objects(if ended_main.is_some() {
                max.saturating_add(1)
            } else {
                max
            })
        };
        if let Some(main) = ended_main {
            threads.retain(|t| *t != main);
        }
        threads.truncate(max);
        threads
    }

    // Interpreter round i2 wave 66, lane L1 (`CRATONVM_DBG_UNRECORDED_WAIT_KINDS`):
    // the kind-less doors count their caller's source line, which
    // `#[track_caller]` (on the trait's declarations too) passes through the
    // `dyn NativeContext` call. Off, one load of a field fixed at
    // construction.
    #[track_caller]
    fn begin_blocking_region(&mut self) {
        note_unrecorded_wait_region(self.shared, "begin_blocking_region");
        self.begin_blocking_region_with_state(1);
    }

    #[track_caller]
    fn begin_timed_blocking_region(&mut self) {
        note_unrecorded_wait_region(self.shared, "begin_timed_blocking_region");
        self.begin_blocking_region_with_state(3);
    }

    fn decline_leaf_call(&mut self) -> bool {
        LEAF_CALL.with(|state| {
            let on_leaf_path = state.get() == LEAF_CALL_RUNNING;
            if on_leaf_path {
                state.set(LEAF_CALL_DECLINED);
            }
            on_leaf_path
        })
    }

    #[track_caller]
    fn begin_blocking_region_as(&mut self, kind: cratonvm_native_api::BlockKind) {
        use cratonvm_native_api::BlockKind;
        // Interpreter round i2 wave 59, lane L6 (`CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE`):
        // a native blocked in C is HotSpot's `_thread_in_native`, RUNNABLE.
        let state = match kind {
            // Wave 66 (lane L1): `Waiting` / `TimedWaiting` say only the
            // state, not the kind: counted as the kind-less doors are.
            BlockKind::Waiting => {
                note_unrecorded_wait_region(self.shared, "begin_blocking_region_as(Waiting)");
                1
            }
            BlockKind::TimedWaiting => {
                note_unrecorded_wait_region(self.shared, "begin_blocking_region_as(TimedWaiting)");
                3
            }
            // Interpreter round i2 wave 63, lane L1 (`CRATONVM_PUBLISHED_WAIT_KIND`):
            // a sleep says so, before `java_state` and the flag
            // (`begin_blocking_region_with_state`), so a reader that sees the
            // flag over TIMED_WAITING sees the kind with it. Lowered by
            // `lower_wait_kind` at the region's end.
            BlockKind::Sleeping => {
                if self.shared.threads.thread_registry.published_wait_kind() {
                    self.thread.gc_block_state.java_wait_kind.store(
                        crate::threading::jvm_thread::JAVA_WAIT_KIND_SLEEPING,
                        std::sync::atomic::Ordering::Release,
                    );
                }
                3
            }
            // Interpreter round i2 wave 64, lane L1 (`CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
            // a region standing for a JDK `wait` loop (the served join) or
            // for JDK code that parks says so, as the sleep does, before
            // `java_state` and the flag; lowered by `lower_wait_kind`.
            BlockKind::ObjectWait
            | BlockKind::TimedObjectWait
            | BlockKind::Parked
            | BlockKind::TimedParked => {
                if self
                    .shared
                    .threads
                    .thread_registry
                    .published_all_wait_kinds()
                {
                    self.thread.gc_block_state.java_wait_kind.store(
                        match kind {
                            BlockKind::ObjectWait | BlockKind::TimedObjectWait => {
                                crate::threading::jvm_thread::JAVA_WAIT_KIND_REGION_OBJECT_WAIT
                            }
                            _ => crate::threading::jvm_thread::JAVA_WAIT_KIND_PARKED,
                        },
                        std::sync::atomic::Ordering::Release,
                    );
                }
                if matches!(kind, BlockKind::TimedObjectWait | BlockKind::TimedParked) {
                    3
                } else {
                    1
                }
            }
            BlockKind::InNative => {
                if self
                    .shared
                    .threads
                    .thread_registry
                    .blocked_in_native_runnable()
                {
                    crate::threading::jvm_thread::JAVA_STATE_IN_NATIVE
                } else {
                    1
                }
            }
        };
        self.begin_blocking_region_with_state(state);
    }

    #[track_caller]
    fn begin_blocking_region_as_on(
        &mut self,
        obj: ObjectRef,
        kind: cratonvm_native_api::BlockKind,
    ) {
        // Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
        // published while still a running mutator (`obj` is live, and the
        // `parkBlocker` store is an ordinary barriered heap write), before the
        // region's state and flag; withdrawn by the region's end.
        self.publish_region_wait_object(obj, kind);
        self.begin_blocking_region_as(kind);
    }

    fn stand_in_wait_objects(&self) -> bool {
        self.shared.threads.thread_registry.stand_in_wait_objects()
    }

    fn begin_native_region(&mut self) {
        // Round 14 wave 1 (lane ffm): `java_state` 0, RUNNABLE while GC-safe,
        // as the JNI in-native bracket (`native::jni::native_call_enter_native`)
        // reports it; used by FFM downcalls.
        // JIT round 16 wave 9b (lane ffm): foreign code from here, before the
        // deposit, so no pause signals the thread while the region is up
        // (`thread_registry::ffm_in_native_signal_exempt`).
        self.thread
            .gc_block_state
            .foreign_native
            .store(true, std::sync::atomic::Ordering::Release);
        self.begin_blocking_region_with_state(0);
    }

    fn begin_vm_wait_region(&mut self) {
        // JIT round 17 wave 2 (lane ffm2): a VM-internal wait (an FFM shared
        // close waiting out in-flight accesses), RUNNABLE as HotSpot's
        // `_thread_in_vm`; not foreign code, so `foreign_native` stays down.
        self.begin_blocking_region_with_state(0);
    }

    fn begin_native_region_quiet(&mut self) -> (u64, u64) {
        // JIT round 16 wave 9 (lane ffm): the JNI in-native bracket's entry
        // (`native::jni::native_call_enter_native`) for an FFM downcall
        // (`CRATONVM_FFM_DOWNCALL_GC_SAFE` with `CRATONVM_FFM_DOWNCALL_IN_NATIVE_QUIET`).
        // Read BEFORE the deposit, so a pause that completes during it makes
        // the leave take the full wake.
        let sync = native_region_sync(self.shared, &self.thread.gc_block_state);
        // JIT round 16 wave 9b (lane ffm): foreign code, as in
        // `begin_native_region` (before the deposit raises the region).
        self.thread
            .gc_block_state
            .foreign_native
            .store(true, std::sync::atomic::Ordering::Release);
        // RUNNABLE while GC-safe, as `begin_native_region`.
        self.thread
            .gc_block_state
            .java_state
            .store(0, std::sync::atomic::Ordering::Release);
        // While still a counted mutator: see `begin_blocking_region_with_state`
        // (no blocked thread keeps a TLAB).
        self.thread.tlab.retire();
        // The stack band above the outermost compiled entry is frozen while
        // the downcall runs, as below a JNI native ("The frozen band of a JNI
        // in-native deposit" in `jit::conservative_roots`): the deposit reuses
        // a clean verdict over it while the JIT entry chain is untouched.
        crate::jit::conservative_roots::with_frozen_band_memo(None, || {
            self.deposit_root_snapshot();
        });
        // No blocked-stack summary (`GcBarrier::mark_in_native_enter`): code
        // reclamation treats the thread as running compiled code, as it treats
        // a downcall with the region off.
        if self.shared.mem.gc_barrier.mark_in_native_enter() {
            // `_auto`: the deposit raised `in_blocked_region` before this check.
            let _ = self
                .shared
                .mem
                .gc_barrier
                .arrive_and_wait_auto(self.thread.thread_id);
        }
        sync
    }

    fn end_native_region_quiet(&mut self, sync: (u64, u64)) {
        // JIT round 16 wave 9 (lane ffm): `native::jni::native_call_leave_native`.
        // One barrier-lock hold: release the count and, if no pause completed
        // and no relocation was folded into this thread since `sync` was read,
        // clear `in_blocked_region` -- the deposit made on the way in still
        // describes every frame (a thread in C changes none of them; an upcall
        // or a JNIEnv call left the region first and re-entered with a fresh
        // deposit and a fresh `sync`), and the fixup is empty.
        // JIT round 16 wave 9b (lane ffm): the C code has returned.
        self.thread
            .gc_block_state
            .foreign_native
            .store(false, std::sync::atomic::Ordering::Release);
        let shared = self.shared;
        let quiet = shared.mem.gc_barrier.leave_in_native_if(
            self.thread.thread_id,
            &self.thread.gc_block_state.in_blocked_region,
            || native_region_sync(shared, &self.thread.gc_block_state) == sync,
        );
        if quiet {
            // Every origin still reads `cur == orig`; the next flag-raising
            // deposit records a fresh set.
            self.thread.gc_block_state.slot_origins.lock().clear();
            crate::runtime::interpreter::obsolete_frames::convert_obsolete_frames_if_redefined(
                shared,
                self.thread,
            );
            return;
        }
        // The flag is still up: the full wake clears it itself.
        self.check_post_block_gc();
    }

    fn begin_native_region_lazy(&mut self, band_lo: usize, band_hi: usize) -> bool {
        self.begin_native_region_lazy_inner(band_lo, band_hi, true)
    }

    fn begin_native_region_lazy_with_locals(&mut self, band_lo: usize, band_hi: usize) -> bool {
        self.begin_native_region_lazy_inner(band_lo, band_hi, false)
    }
    // -- Threading methods --

    fn thread_id(&self) -> u64 {
        self.thread.thread_id.0
    }



    fn end_native_region_lazy(&mut self) {
        // JIT round 16 wave 14 (lane ffm3): one compare-exchange; the slow
        // path waits out the pause that claimed this thread. Conservative
        // claims pin frozen roots; precise claims rewrite interpreter roots
        // before release. In either case Java resumes only after the pause.
        let waited = if self
            .shared
            .threads
            .thread_registry
            .no_roots_lazy_claims_enabled()
        {
            self.thread.gc_block_state.leave_lazy_native()
        } else {
            self.thread
                .gc_block_state
                .leave_lazy_native_at_barrier(&self.shared.mem.gc_barrier)
                .unwrap_or_else(|| self.thread.gc_block_state.leave_lazy_native())
        };
        if waited && None::<std::ffi::OsString>.is_some() {
            eprintln!("[ffm-lazy] native return synchronized with a stop-the-world pause");
        }
        // A redefinition handshake may hold us in C instead of waiting for
        // the call to return. Convert interpreter continuations before Java
        // resumes, just as the quiet native-region leave does. Compiled loops
        // retain their armed exit words and take their post-call/backedge poll.
        crate::runtime::interpreter::obsolete_frames::convert_obsolete_frames_if_redefined(
            self.shared,
            self.thread,
        );
    }

    fn run_in_native_wait_lazy(&mut self, wait: &mut dyn FnMut()) -> bool {
        // Interpreter round i7 wave 139, lane L1b
        // (`CRATONVM_PROCESS_WAIT_LAZY_NATIVE`, default OFF; the caller reads
        // the switch): the lazy in-native protocol of an FFM downcall, entered
        // by `panama_libffi::run_in_native_wait_lazy`, which spills the
        // callee-saved registers and takes the band in its own frame.
        // `begin_native_region_lazy` above declines wherever no take-over can
        // claim the thread, and then nothing ran.
        cratonvm_native_builtins::panama_libffi::run_in_native_wait_lazy(self, wait)
    }

    fn run_blocked_wait(&mut self, wait: &mut dyn FnMut()) {
        // JIT round 18 wave 10 (lane ffmeintr, EINTR2-1): the blocked-wait
        // trampoline over the thread's own block state; `wait()` alone unless
        // `CRATONVM_XT_HELPER_UNPARKED_READ`.
        crate::threading::jvm_thread::run_blocked_wait(&self.thread.gc_block_state, || wait());
    }

    fn sleep_wait_unpolled(&mut self, max: std::time::Duration) -> bool {
        // Interpreter round i3 wave 75, lane L6 (`CRATONVM_SLEEP_NO_POLL`,
        // default off): a platform thread's sleep waits on its own
        // `ParkState` condvar, which `ThreadRegistry::set_interrupted` wakes,
        // instead of in 10 ms slices. A virtual thread keeps the slices (its
        // `ParkState` is shared with the carrier-park protocol).
        if !self.shared.threads.thread_registry.sleep_no_poll()
            || matches!(self.thread.kind, crate::threading::ThreadKind::Virtual)
        {
            return false;
        }
        let park_state = &self.thread.park_state;
        let interrupted = &self.thread.interrupted;
        // Interpreter round i5 wave 109, lane L6
        // (`CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL`, default off): HotSpot's
        // Windows `HighResolutionInterval` for the wait, keyed on its length
        // in whole milliseconds rounded up (`max` is the sleep's remainder,
        // a few microseconds short of the asked length).
        #[cfg(windows)]
        let _interval = self
            .shared
            .threads
            .thread_registry
            .sleep_high_resolution_interval()
            .then(|| {
                let millis =
                    u64::try_from(max.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX);
                crate::threading::jvm_thread::win_park::HighResolutionInterval::new(millis)
            });
        crate::threading::jvm_thread::run_blocked_wait(&self.thread.gc_block_state, || {
            park_state.sleep_unpolled(max, interrupted)
        });
        true
    }

    fn end_blocking_region(&mut self) {
        // JIT round 16 wave 9b (lane ffm): the C code of a `begin_native_region`
        // region has returned (a plain store for every other region, whose
        // flag is already down).
        self.thread
            .gc_block_state
            .foreign_native
            .store(false, std::sync::atomic::Ordering::Release);
        // JDWP (wave 10, lane L1; wave 12): close the debugger's inspection
        // window while still blocked (waiting out a command reading the
        // frames) and withdraw frames published for this region.
        #[cfg(feature = "experimental-debug")]
        crate::runtime::interpreter::close_blocked_inspection(self.shared, self.thread);
        // T19.H1 — leave the blocked region; clear the mark and re-sync
        // with any GC that ran while we were blocked.
        self.shared.mem.gc_barrier.mark_blocked_region_leave();
        // Interpreter round i4 wave 99, lane L1: the quiet leave (keeps the
        // entry's snapshot when nothing it describes changed; the full wake
        // otherwise).
        let _kept = self.check_post_block_gc_quiet_refs(&mut []);
        lower_in_native_state(self.thread);
        lower_wait_kind(self.thread);
        // Wave 68 (lane L1): running again, so the `parkBlocker` clear is a
        // plain heap write through the forwarded mirror.
        self.withdraw_region_wait_object();
    }

    fn end_blocking_region_refs(&mut self, refs: &mut [Value]) {
        // JDWP (wave 12): as in `end_blocking_region`, before the leave.
        #[cfg(feature = "experimental-debug")]
        crate::runtime::interpreter::close_blocked_inspection(self.shared, self.thread);
        // Like `end_blocking_region`, but also rewrites the caller's
        // native-local raw `Value` refs through the accumulated blocked-GC
        // fixup (the `ReferenceQueue.remove` poll receiver would otherwise
        // keep its stale pre-GC address — the stale-receiver writer).
        self.shared.mem.gc_barrier.mark_blocked_region_leave();
        // Wave 99 (lane L1): as in `end_blocking_region`. A kept snapshot
        // means no fixup, so `refs` needed no rewrite either.
        let _kept = self.check_post_block_gc_quiet_refs(refs);
        lower_in_native_state(self.thread);
        lower_wait_kind(self.thread);
        // Wave 68 (lane L1): as in `end_blocking_region`.
        self.withdraw_region_wait_object();
    }

    fn park(&mut self, timeout: Option<std::time::Duration>) {
        self.park_with_state(timeout, false)
    }

    fn park_reporting_timeout(&mut self, timeout: Option<std::time::Duration>) {
        self.park_with_state(timeout, true)
    }

    fn park_until_reporting(&mut self, timeout: Option<std::time::Duration>, until_ms: i64) {
        self.park_with_state_until(timeout, true, Some(until_ms), None)
    }

    fn park_relative_reporting_raw(&mut self, timeout: Option<std::time::Duration>, time_ns: i64) {
        // gci i23/pk: the event's `timeout` is the caller's raw (negative)
        // relative time, as `Unsafe_Park` posts it.
        self.park_with_state_until(timeout, true, None, Some(time_ns))
    }

    fn park_java_untimed(&mut self, timeout: Option<std::time::Duration>) {
        self.park_with_state_inner(timeout, false, None, None, true)
    }

    fn unpark(&self, thread_obj: ObjectRef) {
        // Interpreter round i3 wave 73, lane L6 (`CRATONVM_UNPARK_EETOP`,
        // default off): a platform thread's registry id from its `eetop`.
        if self.shared.threads.thread_registry.unpark_eetop()
            && unpark_platform_through_eetop(self.shared, thread_obj)
        {
            return;
        }
        // Interpreter round i3 wave 72, lane L6 (`CRATONVM_UNPARK_ONE_LOOKUP`,
        // default on since the wave-72 triage): a platform thread's unpark in
        // one registry lookup.
        if self.shared.threads.thread_registry.unpark_one_lookup()
            && unpark_platform_in_one_lookup(self.shared, thread_obj)
        {
            return;
        }
        if let Some(tid) = resolve_thread_id_from_thread_obj(self.shared, thread_obj) {
            if self.shared.threads.virtual_thread_manager.is_virtual(tid.0) {
                self.shared
                    .threads
                    .virtual_thread_manager
                    .cancel_wakeup(tid.0);
                // A PINNED virtual thread does not unmount: it parks on its
                // carrier through its own `ParkState` (the blocking arm of
                // `park`), which a resubmit or a sticky permit never reaches.
                // Pinning used to need a monitor entered by a native; since
                // wave 26 a native beneath the park pins too (`Method.invoke`
                // / `MethodHandle.invoke` of a method that parks). The
                // `ParkState` is signalled only while the thread parks on its
                // carrier (`unpark_virtual_waking`, wave 28): signalled always,
                // it was a second permit that ended the thread's next carrier
                // park at once.
                // Wave 91 (lane L6, page
                // `i77-L6-proposal-every-vm-unpark-through-the-shape`): the
                // carrier-park wake takes the VM's unpark shape.
                let registry = &self.shared.threads.thread_registry;
                let park_state = registry.get_park_state(tid);
                self.shared
                    .threads
                    .virtual_thread_manager
                    .unpark_virtual_waking_with(
                        tid.0,
                        park_state.as_deref(),
                        registry.unpark_shape(),
                    );
                return;
            }
        }
        // Find the ParkState associated with this Java Thread object
        // by looking up in the thread registry.
        if let Some(park_state) = self.shared.find_park_state_for_thread_obj(thread_obj) {
            unpark_park_state(self.shared, &park_state);
        } else if crate::runtime::env_cache::dbg_unpark_miss() {
            // DBG: a miss here is a SILENTLY LOST WAKEUP — the caller's
            // Thread-mirror address matched no registered thread. Print the
            // stale address, what its header currently claims to be, and the
            // registry's live mirror addresses so the delta (e.g. a young
            // address whose thread was promoted to old gen) is visible.
            let class_id = self.shared.mem.heap.class_id_of(thread_obj);
            eprintln!(
                "[unpark] MISS obj={:p} header_class_id={:?} registry={:x?}",
                thread_obj.as_ptr(),
                class_id,
                self.shared.threads.thread_registry.debug_thread_obj_addrs(),
            );
        }
    }

    // -- Scoped Values (JEP 446, Java 25) --

    fn get_scoped_value(&self, key_id: u64) -> Option<Value> {
        // Search top-to-bottom for matching key_id
        for (k, _key_ref, v) in self.thread.scoped_values.iter().rev() {
            if *k == key_id {
                return Some(*v);
            }
        }
        None
    }

    fn push_scoped_value(&mut self, key_id: u64, value: Value) {
        // Legacy API: no key ObjectRef supplied. Round-9 GC fix prefers
        // `push_scoped_value_with_key` so the key itself is GC-pinned.
        self.thread.scoped_values.push((key_id, None, value));
    }

    fn push_scoped_value_with_key(
        &mut self,
        key_id: u64,
        key_ref: Option<cratonvm_types::ObjectRef>,
        value: Value,
    ) {
        self.thread.scoped_values.push((key_id, key_ref, value));
    }

    fn pop_scoped_value(&mut self) {
        self.thread.scoped_values.pop();
    }

    fn scoped_value_depth(&self) -> usize {
        self.thread.scoped_values.len()
    }
}

/// JIT round 16 wave 14 (lane ffm3): can a collection claim a thread that went
/// into C lazily (`NativeContextImpl::begin_native_region_lazy`)? Only where
/// the take-over runs (`xt_root_scan::enabled` and a heap that collects while a
/// frozen peer keeps its TLAB, `stw_take_over_and_wait`'s own gate), on the
/// targets whose callee-saved register spill `panama_libffi` implements.
/// Elsewhere every pause would wait for the thread: the caller falls back.
/// JIT round 18 wave 10 (lane ffmeintr): the VM half of
/// `cratonvm_native_api::registry::NativeAllocNoGcPoint` ("an allocation is
/// not a GC point", `r17-native-alloc-gc-point-rule-differs-between-api-and-moving-mock`).
/// Each no-GC allocation door of `impl NativeHeapAccess for NativeContextImpl`
/// (`alloc_object`, `try_alloc_object`, `new_array`, `try_new_array`,
/// `new_ref_array`, `try_new_ref_array`, `create_string`) takes it first; the
/// guard panics at the door's end if a stop-the-world pause COMPLETED inside it
/// (`GcBarrier::gc_generation` moved), naming the door.
///
/// Debug builds only, and only with `CRATONVM_DBG_NATIVE_ALLOC_NO_GC_POINT=1`:
/// in a release build `cfg!(debug_assertions)` is a constant `false`, the flag
/// is never read and the `Option` of a zero-sized guard folds away, so the
/// doors compile to what they were. The switch keeps the default debug test
/// suites unaffected: the check is exact only for a COUNTED mutator, and a
/// unit test that runs a native on an unregistered test thread while another
/// thread of the same VM collects would trip it without any door being wrong.
#[inline(always)]
pub(in crate::vm::vm_exec) fn native_alloc_no_gc_point<'a>(
    door: &'static str,
    shared: &'a SharedVm,
) -> Option<cratonvm_native_api::registry::NativeAllocNoGcPoint<'a>> {
    if cfg!(debug_assertions)
        && false
    {
        Some(cratonvm_native_api::registry::NativeAllocNoGcPoint::enter(
            door,
            &shared.mem.gc_barrier.gc_generation,
        ))
    } else {
        None
    }
}

pub(in crate::vm::vm_exec) fn lazy_native_claimable(shared: &SharedVm) -> bool {
    cfg!(all(
        target_arch = "x86_64",
        any(target_os = "windows", target_os = "linux")
    )) && crate::jit::xt_root_scan::enabled()
        && shared.mem.heap.supports_jit_tlab_skip()
}

/// JIT round 16 wave 9 (lane ffm): "has anything happened to this thread's
/// roots since it went into native?" for
/// `NativeContextImpl::begin_native_region_quiet` / `end_native_region_quiet`:
/// the VM's pause generation (any completed pause) and the thread's fold count
/// (any relocation folded into its blocked state). The JNI bracket's
/// `native::jni::NativeSync`, as a pair (the trait answer is opaque).
pub(in crate::vm::vm_exec) fn native_region_sync(
    shared: &SharedVm,
    block: &crate::threading::jvm_thread::GcBlockState,
) -> (u64, u64) {
    (
        shared
            .mem
            .gc_barrier
            .gc_generation
            .load(std::sync::atomic::Ordering::Acquire),
        block
            .blocked_folds
            .load(std::sync::atomic::Ordering::Acquire),
    )
}

// Interpreter round i4 wave 99, lane L1 -- the QUIET LEAVE of a blocking region
// (`CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT`, default on; page
// `docs/feature-designs/real-jdk-mode.md`).
//
// Every `end_blocking_region*` used to run the wake's frame audit and a FULL
// root-snapshot rebuild (`deposit_root_snapshot_no_flag_with`) even when nothing
// happened in the window: ~100 us on the leave of every park, sleep, wait,
// socket and process wait, growing with stack depth. The quiet leave keeps the
// snapshot the entry deposited instead, when a rebuild would publish the same
// roots. The argument:
//
// A. Who reads the snapshot after the leave. With the flag down the thread is a
//    counted mutator. No root-gathering pause reads its snapshot instead of a
//    fresh publish: a pause completes only when each counted mutator arrived
//    through a door that republishes, or was frozen by the take-over (items 1-2
//    of the lazy-WAKE section in `jit::conservative_roots`). The take-over of a
//    peer frozen in compiled code does read the snapshot, as the view of the
//    peer's INTERPRETER frames (`root_snapshots_for_os_tids`,
//    `collect_all_root_snapshots`), and diagnostics read it outside a pause. So
//    the kept snapshot is sound exactly when, at the leave, it publishes what a
//    rebuild would (up to over-retention, which the next publish drops).
// B. What the snapshot is a function of: the frames (slots, monitors, class
//    owners), the off-frame `JvmThread` roots, the JNI local references, the
//    JIT's deopt stash, the compiled chain and the band above it, the shadow
//    stack, the heap's answers for conservative words, and configuration. The
//    leave compares a [`BlockingLeaveKey`] read at the entry with one read after
//    the flag-clear, and refuses on any difference. Each invalidating event:
//    1. A collection, compaction or remap folded into the window: the pause
//       generation and collection count (read BEFORE the entry's deposit, as the
//       JNI quiet leave reads its sync), the thread's fold count; and in the
//       leave `folds_k == 0` and an empty fixup. Any completed pause moves the
//       generation, a non-moving one too.
//    2. The native touching frames, handles or locals: a frame pushed or popped
//       moves `frames_len`; the owner can change no frame but the top one
//       without that, and the top frame's `(seq, exec_epoch, pc, stack depth)`
//       moves with a store, a push or a pop. Pins, handles, the native
//       allocation pool, the pending native return, scoped values, the parked
//       throwables, the memo caches, the mirror, the JNI locals and the deopt
//       stash are compared as the exact list of addresses (not a hash).
//    3. Deopt: compiled code cannot run inside the region (the thread is in the
//       native), so its frames neither deopt nor stash; an invalidation another
//       thread requests converts at the thread's return into that code, after
//       the leave. The stash itself is in the compared list.
//    4. A stack-band change: the compiled chain is frozen for the same reason.
//       The band above it differs only in the Rust frames of the blocking door
//       (`begin_blocking_region*` at the entry, `end_blocking_region*` now),
//       whose words are over-retention in either snapshot; a reference the
//       native keeps in its own frame was there at the entry too (no heap access
//       while GC-blocked). An entry that DEFERRED its chain to the collector
//       left the chain out of the snapshot: the lazy-wake rebuild leaves it out
//       as well, so the keep is the same snapshot; without the lazy wake the
//       leave rebuilds.
//    5. Thread suspension, JVMTI / JDWP frame edits: an object written into a
//       blocked thread's frame is deferred to its wake (the VM-wide
//       `frame_writes_pending`): rebuild. A C JVMTI primitive `SetLocal*` writes
//       in place and can only REMOVE a reference (over-retention until the next
//       publish). A suspension takes effect at the thread's next suspend point,
//       after the leave, which publishes.
//    6. A class redefinition: the redefinition count, and the leave also
//       refuses while the thread has a conversion pending
//       (`redefinitions_seen`), since `convert_obsolete_frames_if_redefined`
//       runs after the decision and rewrites frames.
//    7. Conservative scans: a word's verdict changes only when its object is
//       reclaimed (a collection: 1.) or when an object is newly allocated at an
//       address a non-reference word happens to equal (a false root the kept
//       snapshot lacks: nothing lost).
//    8. Another deposit in the window (a nested region, a JNI in-native entry,
//       the debugger park's reuse): `root_publish_seq`. Every door deposits and
//       only `begin_blocking_region_with_state` records a key (every wake takes
//       it), so a key cannot outlive its window or describe another one.
//    Refused outright: foreign code (an FFM downcall region: C may upcall),
//    a terminating thread, `CRATONVM_DBG_BLOCKGC` (keeps its diagnostics).
// C. What the kept leave does not redo, and why that is consistent: the pins
//    and `deposit_conservative` the entry published pair with the kept snapshot;
//    the blocked trace is published only by flag-raising deposits anyway; the
//    reclaim guard's last publish stays the entry's (same collection count, same
//    top pc); the wake audit is skipped (no collection, so nothing was reclaimed
//    in the window, and the entry audited the same frames). `root_publish_seq`
//    is not bumped: the entry's bump already invalidated the JNI repeat and the
//    debugger park's standing key. The rest of the wake (slot origins, native
//    slot fixups, deferred-write apply, obsolete conversion, RUNNABLE status)
//    runs as always.
// Precedents: the JNI/FFM quiet in-native leave (`end_native_region_quiet`),
// which keeps on the pause generation and the fold count alone because C changes
// no frame; and the lazy wake (`CRATONVM_GC_LAZY_BLOCKED_JIT_WAKE`). The wave-68
// park keep (`check_post_block_gc_keeping`) was KEPT OFF for its debugger park
// switches' timings (`L6W68`), not for soundness; it is unchanged.

/// Wave 99 (lane L1): the pause terms of a `jvm_thread::BlockingLeaveKey`
/// (generation, collections, folds, redefinitions). Read at the entry BEFORE
/// its deposit, and at the leave after the flag-clear.
#[derive(Clone, Copy)]
pub(in crate::vm::vm_exec) struct BlockingLeavePauseTerms {
    pub(in crate::vm::vm_exec) gc_generation: u64,
    pub(in crate::vm::vm_exec) collections: u64,
    pub(in crate::vm::vm_exec) blocked_folds: u64,
    pub(in crate::vm::vm_exec) redefinitions: u64,
}

pub(in crate::vm::vm_exec) fn blocking_leave_pause_terms(shared: &SharedVm, thread: &JvmThread) -> BlockingLeavePauseTerms {
    BlockingLeavePauseTerms {
        gc_generation: shared
            .mem
            .gc_barrier
            .gc_generation
            .load(std::sync::atomic::Ordering::Acquire),
        collections: shared.mem.heap.collection_count(),
        blocked_folds: thread
            .gc_block_state
            .blocked_folds
            .load(std::sync::atomic::Ordering::Acquire),
        redefinitions: cratonvm_classloading::class_redefinition_count(),
    }
}

/// Wave 99 (lane L1): the quiet-leave key of `thread` now, with `terms` (see
/// the section above for what it covers). Cost: the off-frame roots and the
/// top frame, independent of the stack depth.
pub(in crate::vm::vm_exec) fn blocking_leave_key(
    terms: BlockingLeavePauseTerms,
    thread: &JvmThread,
) -> crate::threading::jvm_thread::BlockingLeaveKey {
    let mut roots: Vec<ObjectRef> = Vec::new();
    crate::memory::roots::push_off_frame_thread_roots(thread, &mut roots);
    crate::native::jni::collect_local_ref_roots(&mut roots);
    let mut off_frame: Vec<usize> = roots.iter().map(|o| o.as_ptr() as usize).collect();
    // A separator, so a root moving between the lists cannot read as equal.
    off_frame.push(usize::MAX);
    cratonvm_jit::deopt::for_each_stashed_deopt_object(|addr| off_frame.push(addr as usize));
    blocking_leave_key_head(terms, thread, off_frame)
}

/// The scalar terms of a [`blocking_leave_key`], around a given `off_frame`
/// list (empty for a comparison of the scalars alone).
pub(in crate::vm::vm_exec) fn blocking_leave_key_head(
    terms: BlockingLeavePauseTerms,
    thread: &JvmThread,
    off_frame: Vec<usize>,
) -> crate::threading::jvm_thread::BlockingLeaveKey {
    crate::threading::jvm_thread::BlockingLeaveKey {
        gc_generation: terms.gc_generation,
        collections: terms.collections,
        blocked_folds: terms.blocked_folds,
        redefinitions: terms.redefinitions,
        publish_seq: thread
            .root_publish_seq
            .load(std::sync::atomic::Ordering::Relaxed),
        frames_len: thread.frames.len(),
        top_frame: thread.frames.last().map_or([0; 4], |f| {
            [f.seq, f.exec_epoch, f.pc as u64, f.stack.len() as u64]
        }),
        off_frame,
    }
}

// Interpreter round i5 wave 100, lane L1
// (`CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS`, default off; proposal B of
// `interpreter-i100-L1-proposal-a-blocked-thread-is-walked-not-deposited-CLOSED-20261224.md`):
// the same key, the same EXACT comparison, without the two `Vec` allocations
// per build and the leave's second list. The entry fills a recycled list; the
// leave streams the current roots against the entry's list (scalars first,
// stopping at the first difference) and recycles it. Nothing is hashed, so
// the answer is exactly `blocking_leave_key(..) == entry`. The buffers are
// the owning thread's scratch: the key is built and compared only by its own
// thread (`GcBlockState::blocking_leave_key`), and the scratch root list is
// cleared before it is put back, so it never holds an address between uses.
thread_local! {
    pub(in crate::vm::vm_exec) static LEAVE_KEY_ROOTS: std::cell::Cell<Vec<ObjectRef>> =
        const { std::cell::Cell::new(Vec::new()) };
    pub(in crate::vm::vm_exec) static LEAVE_KEY_SPARE: std::cell::Cell<Vec<usize>> =
        const { std::cell::Cell::new(Vec::new()) };
}

/// Run `f` over this thread's off-frame roots (the `JvmThread` fields, then
/// the JNI local references: [`blocking_leave_key`]'s order), in the
/// recycled scratch list.
pub(in crate::vm::vm_exec) fn with_leave_key_roots<R>(thread: &JvmThread, f: impl FnOnce(&[ObjectRef]) -> R) -> R {
    let mut roots = LEAVE_KEY_ROOTS.with(|cell| cell.take());
    roots.clear();
    crate::memory::roots::push_off_frame_thread_roots(thread, &mut roots);
    crate::native::jni::collect_local_ref_roots(&mut roots);
    let answer = f(&roots);
    roots.clear();
    LEAVE_KEY_ROOTS.with(|cell| cell.set(roots));
    answer
}

/// [`blocking_leave_key`] into a recycled list.
pub(in crate::vm::vm_exec) fn blocking_leave_key_reusing(
    terms: BlockingLeavePauseTerms,
    thread: &JvmThread,
) -> crate::threading::jvm_thread::BlockingLeaveKey {
    let mut off_frame = LEAVE_KEY_SPARE.with(|cell| cell.take());
    off_frame.clear();
    with_leave_key_roots(thread, |roots| {
        off_frame.extend(roots.iter().map(|o| o.as_ptr() as usize));
    });
    off_frame.push(usize::MAX);
    cratonvm_jit::deopt::for_each_stashed_deopt_object(|addr| off_frame.push(addr as usize));
    blocking_leave_key_head(terms, thread, off_frame)
}

/// `blocking_leave_key(terms, thread) == entry`, streamed: no list is built.
/// Recycles the entry's list.
pub(in crate::vm::vm_exec) fn blocking_leave_key_matches(
    mut entry: crate::threading::jvm_thread::BlockingLeaveKey,
    terms: BlockingLeavePauseTerms,
    thread: &JvmThread,
) -> bool {
    let expected = std::mem::take(&mut entry.off_frame);
    let mut same = blocking_leave_key_head(terms, thread, Vec::new()) == entry;
    if same {
        let roots_len = with_leave_key_roots(thread, |roots| {
            same = roots.len() < expected.len()
                && roots
                    .iter()
                    .zip(expected.iter())
                    .all(|(o, &addr)| o.as_ptr() as usize == addr);
            roots.len()
        });
        if same && expected[roots_len] != usize::MAX {
            same = false;
        }
        if same {
            let mut at = roots_len + 1;
            let mut stash_same = true;
            cratonvm_jit::deopt::for_each_stashed_deopt_object(|addr| {
                if expected.get(at) != Some(&(addr as usize)) {
                    stash_same = false;
                }
                at += 1;
            });
            same = stash_same && at == expected.len();
        }
    }
    LEAVE_KEY_SPARE.with(|cell| cell.set(expected));
    same
}

/// The class-manager read a throwable capture takes lazily, at most once.
pub(in crate::vm::vm_exec) type CaptureClassManager<'a> =
    crate::runtime::lock_order::OrderedPlRwLockReadGuard<'a, crate::classloading::ClassManager>;

/// `stackwalker::throwable_holder_is_a` for the throwable of class
/// `throwable_class`, the fill-frame trim's question. The class manager is
/// read into `cm` the first time a frame's name asks it, and kept for the rest
/// of the capture.
pub(in crate::vm::vm_exec) fn throwable_capture_is_a<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    throwable_class: ClassId,
    holder: Option<ClassId>,
    holder_name: &str,
) -> bool {
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let Some(tclass) = cm.class_store.get(throwable_class) else {
        return false;
    };
    crate::runtime::stackwalker::throwable_holder_is_a(&cm.class_store, tclass, holder, holder_name)
}

/// [`throwable_capture_is_a`] through the thread's
/// `stackwalker::ThrowableIsAMemo` (JIT round 16 wave 11, lane throw2,
/// TP16-12): a remembered `(throwable class, holder id)` answer is given
/// without the class-manager lock; a fresh one is remembered. `memo` is `None`
/// with `CRATONVM_THROWABLE_TRIM_IS_A_MEMO` off, and a holder without a class
/// id is never memoised -- both are exactly [`throwable_capture_is_a`].
pub(in crate::vm::vm_exec) fn throwable_capture_is_a_memoised<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    memo: Option<&mut crate::runtime::stackwalker::ThrowableIsAMemo>,
    redefinitions: u64,
    throwable_class: ClassId,
    holder: Option<ClassId>,
    holder_name: &str,
) -> bool {
    let (Some(memo), Some(holder_id)) = (memo, holder) else {
        return throwable_capture_is_a(shared, cm, throwable_class, holder, holder_name);
    };
    if let Some(answer) = memo.lookup(redefinitions, throwable_class, holder_id) {
        return answer;
    }
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let Some(tclass) = cm.class_store.get(throwable_class) else {
        return false;
    };
    let answer = crate::runtime::stackwalker::throwable_holder_is_a(
        &cm.class_store,
        tclass,
        holder,
        holder_name,
    );
    memo.remember(redefinitions, throwable_class, holder_id, answer);
    answer
}

/// Leave the hidden frames (`Method::is_hidden`) out of a `--real-jdk`
/// throwable trace; how many were left out. The class manager is read (into
/// `cm`) only while some frame is still undecided: an interpreted method once,
/// a compiled or inlined frame of a JDK or hidden class through its label's
/// memo (interpreter round i1 wave 37). One pass when nothing is hidden and
/// everything is decided. `CRATONVM_DBG_STTRACE=1` names each frame left out
/// and its shape (`STTRACE_DBG_HIDDEN kind=compiled|inlined|interpreted|entry`).
pub(in crate::vm::vm_exec) fn drop_hidden_backtrace_frames<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
) -> usize {
    use crate::runtime::stackwalker::{
        backtrace_frame_hidden_decided, backtrace_frame_is_hidden, backtrace_frame_kind,
        BacktraceFrame,
    };
    let mut undecided = false;
    let mut any_hidden = false;
    for frame in trace.iter() {
        match backtrace_frame_hidden_decided(frame) {
            None => undecided = true,
            Some(true) => any_hidden = true,
            Some(false) => {}
        }
    }
    if !undecided && !any_hidden {
        return 0;
    }
    let before = trace.len();
    let dbg = crate::runtime::env_cache::dbg_sttrace();
    let keep = |frame: &BacktraceFrame, hidden: bool| {
        if hidden && dbg {
            let e = frame.to_entry();
            eprintln!(
                "STTRACE_DBG_HIDDEN kind={} {}.{}",
                backtrace_frame_kind(frame),
                e.class_name,
                e.method_name
            );
        }
        !hidden
    };
    if undecided {
        let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
        let store = &cm.class_store;
        trace.retain(|frame| keep(frame, backtrace_frame_is_hidden(store, frame)));
    } else {
        trace.retain(|frame| keep(frame, backtrace_frame_hidden_decided(frame) == Some(true)));
    }
    before - trace.len()
}

/// `NativeContext::hidden_stack_entries` (`show` = this VM's
/// `-XX:+ShowHiddenFrames`) and `NativeContext::hidden_member_entries`
/// (`show` = `false`): which of `entries` a walk leaves out. Under `show` no
/// real method is left out (interpreter round i4 wave 84, lane L2), only the
/// VM's own bridge frame. Split out of `hidden_stack_entries` by interpreter
/// round i4 wave 92, lane L2, so the member view does not follow the flag.
pub(in crate::vm::vm_exec) fn hidden_entries_view(shared: &SharedVm, entries: &[StackTraceEntry], show: bool) -> Vec<bool> {
    use crate::runtime::stackwalker::{
        entry_is_vm_bridge, entry_may_be_hidden, stack_entry_is_hidden,
    };
    // `--real-jdk` (interpreter round i1 wave 35), and `--compatible` under
    // `VmConfig::compatible_hides_hidden_frames` (wave 48). Round 15 wave 3
    // hand-back (lane trace4): `--compatible` with that switch off still
    // hides `java.lang.Thread.runWith` alone, as its throwable capture does
    // (`drop_mode_hidden_frames`, same switches). JIT round 16 wave 8
    // (lane trace): and the compiled indy bridge's synthetic frame, in
    // every mode (`stackwalker::entry_is_vm_bridge`,
    // `CRATONVM_THROWABLE_HIDE_INDY_BRIDGE`); the other arm hides it
    // through `stack_entry_is_hidden`.
    let models_hidden =
        shared.config.is_jdk_only() || true;
    if show || !models_hidden {
        let run_with = |e: &StackTraceEntry| {
            &*e.class_name == "java/lang/Thread" && &*e.method_name == "runWith"
        };
        // `-XX:+ShowHiddenFrames` keeps `runWith` (interpreter round i4
        // wave 84, lane L2); the VM's own bridge frame stays hidden.
        let hide_run_with = !show
            && entries.iter().any(run_with);
        if hide_run_with || entries.iter().any(entry_is_vm_bridge) {
            return entries
                .iter()
                .map(|e| (hide_run_with && run_with(e)) || entry_is_vm_bridge(e))
                .collect();
        }
        return vec![false; entries.len()];
    }
    // The class manager is read once per walk, and only when some frame's
    // class can be hidden at all.
    if !entries
        .iter()
        .any(|e| e.class_id.is_some() && entry_may_be_hidden(&e.class_name))
    {
        return vec![false; entries.len()];
    }
    let cm = shared.classes.class_manager.read();
    let hidden: Vec<bool> = entries
        .iter()
        .map(|e| stack_entry_is_hidden(&cm.class_store, e))
        .collect();
    drop(cm);
    if crate::runtime::env_cache::dbg_sttrace() {
        for (e, _) in entries.iter().zip(&hidden).filter(|(_, h)| **h) {
            eprintln!(
                "STTRACE_DBG_HIDDEN kind=walk {}.{}",
                e.class_name, e.method_name
            );
        }
    }
    hidden
}

/// `NativeContext::hidden_standin_frames`: the hidden JDK frames HotSpot runs
/// and this VM does not push, for `frames` (INNERMOST first), as `(index,
/// entries)` insertions (ascending; each before `frames[index]`). The lambda
/// proxy's SAM frame (interpreter round i1 wave 50, lane L3;
/// `VmConfig::standin_lambda_proxy_frames`) and the method-handle frames of a
/// call the VM serves natively (wave 48; `VmConfig::standin_hidden_invoke_frames`).
/// Fail-closed: a shape the tables do not know gets nothing. Loads the
/// invoke holder classes, so it must be called with no class-manager guard
/// held. A free function since interpreter round i4 wave 92, lane L2, so the
/// throwable capture can ask it under `-XX:+ShowHiddenFrames`.
pub(in crate::vm::vm_exec) fn hidden_standin_insertions(
    shared: &SharedVm,
    frames: &[StackTraceEntry],
) -> Vec<(usize, Vec<StackTraceEntry>)> {
    // Interpreter round i1 wave 50, lane L3: the lambda proxy's SAM
    // frame between a call site's `invokeinterface` and the
    // implementation method the VM's lambda dispatch ran directly.
    // The proxies are copied out first and their names read with no
    // lock held, so no two of the three locks nest.
    let proxies = {
        let sites: Vec<std::sync::Arc<crate::classloading::resolution::LambdaCallSite>> = shared
            .classes
            .lambda_proxies
            .read()
            .values()
            .filter(|site| {
                frames
                    .iter()
                    .any(|f| crate::runtime::stackwalker::lambda_site_stands_below(site, f))
            })
            .cloned()
            .collect();
        let candidates: Vec<crate::runtime::stackwalker::LambdaProxyCandidate> = sites
            .into_iter()
            .filter_map(|site| {
                let id = site.proxy_class_id;
                let (_, host) = crate::runtime::interpreter::lambda_proxy_host(shared, id)?;
                let name: std::sync::Arc<str> =
                    std::sync::Arc::from(format!("{host}$$Lambda/0x{:016x}", id.as_u32()));
                Some(crate::runtime::stackwalker::LambdaProxyCandidate { site, name })
            })
            .collect();
        if candidates.is_empty() {
            Vec::new()
        } else {
            let cm = shared.classes.class_manager.read();
            crate::runtime::stackwalker::lambda_proxy_standin_frames(
                &cm.class_store,
                frames,
                &candidates,
            )
        }
    };
    // Interpreter round i1 wave 48, lane L3: the hidden method-handle
    // frames of a call the VM serves natively, read from the one stand-in
    // table. The holder classes they name are loaded first (HotSpot has
    // them loaded from its archive; this VM never links a LambdaForm).
    for class in crate::runtime::stackwalker::HIDDEN_INVOKE_HOLDER_CLASSES {
        let _ = shared.load_class_concurrent(class);
    }
    // JIT round 22 wave 1, lane runtime (flip review R1 / R3): the adapters
    // the linked reflective calls skip, read before the class-manager guard.
    // Wave 2: only under the synthesis
    // (`CRATONVM_JIT_REFLECT_LINK_HIDDEN_STANDINS`, default ON since j27
    // 2026-10-28); at `=0` the link declines once hidden frames can be
    // observed (the latch), so no linked shape is left.
    let linked: Vec<crate::vm::realms::jit_realm::ReflectLinkAdapter> =
        if crate::jit::helpers::reflect_link_hidden_standins_enabled(shared) {
            shared
                .jit
                .reflect_link_ids
                .adapters
                .read()
                .values()
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
    let cm = shared.classes.class_manager.read();
    let invoke = crate::runtime::stackwalker::hidden_invoke_standin_frames_with(
        &cm.class_store,
        frames,
        &linked,
    );
    crate::runtime::stackwalker::merge_standin_insertions(proxies, invoke)
}

/// `NativeExceptionAccess::caller_lambda_proxy_insertions`: the lambda proxy
/// SAM frames HotSpot runs between `frames` (INNERMOST first) and that a
/// caller-class question must see, since JDK 25's `JVM_GetCallerClass` does not
/// skip a hidden proxy frame (a method reference `Field::get` makes the proxy
/// `Host$$Lambda/0x..` the caller of `Field.get`). Interpreter round i4 wave
/// 94, lane L3 (`L3W94CallerSensitiveMatrix` rows `get-other-lambda`,
/// `newinst-sha-lambda`; `VmConfig::caller_lambda_proxy_frames`). Two shapes,
/// both fail-closed:
///
/// * the caller-sensitive method has a frame (a JDK body): the walk's own
///   proxy rule (`stackwalker::lambda_proxy_standin_frames`);
/// * it is a native the VM serves (no frame): the innermost frame stands at an
///   `invokeinterface` of a SAM that exactly ONE known proxy implements with a
///   method `served(class, name)` accepts; that proxy's frame goes on top.
///
/// `served_only`: the second shape alone (no proxy table scan unless the
/// innermost frame stands at an `invokeinterface`).
pub(in crate::vm::vm_exec) fn caller_lambda_proxy_insertions(
    shared: &SharedVm,
    frames: &[StackTraceEntry],
    served: &dyn Fn(&str, &str) -> bool,
    served_only: bool,
) -> Vec<(usize, Vec<StackTraceEntry>)> {
    use crate::classloading::resolution::LambdaCallSite;
    let Some(top) = frames.first() else {
        return Vec::new();
    };
    let top_call: Option<(String, String, String)> = {
        let cm = shared.classes.class_manager.read();
        crate::runtime::stackwalker::interface_call_at(&cm.class_store, top)
            .map(|(i, n, d)| (i.to_string(), n.to_string(), d.to_string()))
    };
    if served_only && top_call.is_none() {
        return Vec::new();
    }
    let served_site = |site: &LambdaCallSite| {
        top_call.as_ref().is_some_and(|(i, n, d)| {
            *site.functional_interface == **i
                && *site.sam_method_name == **n
                && *site.sam_descriptor == **d
                && served(&*site.impl_handle.class_name, &*site.impl_handle.member_name)
        })
    };
    let sites: Vec<std::sync::Arc<LambdaCallSite>> = shared
        .classes
        .lambda_proxies
        .read()
        .values()
        .filter(|site| {
            served_site(&***site)
                || (!served_only
                    && frames
                        .iter()
                        .any(|f| crate::runtime::stackwalker::lambda_site_stands_below(site, f)))
        })
        .cloned()
        .collect();
    if sites.is_empty() {
        return Vec::new();
    }
    let candidates: Vec<crate::runtime::stackwalker::LambdaProxyCandidate> = sites
        .into_iter()
        .filter_map(|site| {
            let id = site.proxy_class_id;
            let (_, host) = crate::runtime::interpreter::lambda_proxy_host(shared, id)?;
            let name: std::sync::Arc<str> =
                std::sync::Arc::from(format!("{host}$$Lambda/0x{:016x}", id.as_u32()));
            Some(crate::runtime::stackwalker::LambdaProxyCandidate { site, name })
        })
        .collect();
    let mut out = {
        let cm = shared.classes.class_manager.read();
        crate::runtime::stackwalker::lambda_proxy_standin_frames(&cm.class_store, frames, &candidates)
    };
    let mut served_candidates = candidates.iter().filter(|c| served_site(&*c.site));
    if let (Some(only), None) = (served_candidates.next(), served_candidates.next()) {
        if let Some(entry) =
            crate::runtime::stackwalker::lambda_proxy_standin_entry(&only.site, &only.name)
        {
            if out.first().is_none_or(|(at, _)| *at != 0) {
                out.insert(0, (0, vec![entry]));
            }
        }
    }
    out
}

/// Under `-XX:+ShowHiddenFrames` (`VmConfig::show_hidden_frames`): insert
/// into a throwable's `trace` (OUTERMOST first) the hidden JDK frames HotSpot
/// lists there and this VM never pushed ([`hidden_standin_insertions`]: a
/// lambda proxy's SAM frame, the `LambdaForm$MH` / `DirectMethodHandle$Holder`
/// / `Invokers$Holder` frames of a `MethodHandle` or reflective call the VM
/// serves natively), as `java_lang_Throwable::fill_in_stack_trace` lists
/// them with `skip_hidden == false`; then re-cap at `max_depth`. Answers
/// whether anything was inserted. Interpreter round i4 wave 92, lane L2
/// (`i84-L2-show-hidden-frames-lists-no-frames-the-vm-never-pushed`). Call
/// with no class-manager guard held.
pub(in crate::vm::vm_exec) fn insert_show_hidden_standin_frames(
    shared: &SharedVm,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
    max_depth: usize,
) -> bool {
    use crate::runtime::stackwalker::BacktraceFrame;
    if trace.len() < 2 {
        return false;
    }
    // Innermost first, as the stand-in tables read a walk's frames.
    let entries: Vec<StackTraceEntry> = trace.iter().rev().map(BacktraceFrame::to_entry).collect();
    let insertions = hidden_standin_insertions(shared, &entries);
    if insertions.is_empty() {
        return false;
    }
    let innermost_first: Vec<BacktraceFrame> = std::mem::take(trace).into_iter().rev().collect();
    let mut spliced: Vec<BacktraceFrame> = Vec::with_capacity(innermost_first.len() + 4);
    let mut pending = insertions.into_iter().peekable();
    for (i, frame) in innermost_first.into_iter().enumerate() {
        while let Some((_, standins)) = pending.next_if(|(at, _)| *at <= i) {
            spliced.extend(standins.into_iter().map(BacktraceFrame::Entry));
        }
        spliced.push(frame);
    }
    for (_, standins) in pending {
        spliced.extend(standins.into_iter().map(BacktraceFrame::Entry));
    }
    spliced.reverse();
    *trace = spliced;
    crate::runtime::stackwalker::cap_throwable_trace(trace, max_depth);
    true
}

/// The hidden frames a throwable capture leaves out, per mode: every one
/// under `--real-jdk` ([`drop_hidden_backtrace_frames`]); under `--compatible`
/// only `java.lang.Thread.runWith` (round 15 wave 3 hand-back, lane trace4).
/// That `@Hidden` method runs as real bytecode in both modes under a bound
/// virtual thread (`ThreadBuilders$BoundVirtualThread.run` calls it to run
/// its task), and HotSpot never lists it; with the served `Thread.run` rows
/// the `--compatible` trace of a virtual thread whose task is a `Thread`
/// differed from HotSpot by that frame alone. The general `--compatible`
/// hidden drop is taken only under `VmConfig::compatible_hides_hidden_frames`
/// (`CRATONVM_COMPATIBLE_HIDDEN_FRAMES`, interpreter round i1 wave 48, lane
/// L3; `VmConfig::hides_hidden_frames`); with it off, `runWith` alone.
/// `CRATONVM_THROWABLE_COMPAT_HIDDEN_RUN_WITH=0` keeps the
/// frame; read only once a trace holds one. Answers how many were dropped.
pub(in crate::vm::vm_exec) fn drop_mode_hidden_frames<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
) -> usize {
    use crate::runtime::stackwalker::is_thread_run_with_frame;
    // Every hidden frame under `--real-jdk`, and under `--compatible` with
    // `VmConfig::compatible_hides_hidden_frames` (interpreter round i1 wave
    // 48, lane L3); otherwise (round 15 wave 3) `Thread.runWith` alone.
    if shared.config.hides_hidden_frames() {
        return drop_hidden_backtrace_frames(shared, cm, trace);
    }
    // `-XX:+ShowHiddenFrames` keeps `runWith` too (interpreter round i4 wave
    // 84, lane L2).
    if shared.config.show_hidden_frames
        || !trace.iter().any(is_thread_run_with_frame)
    {
        return 0;
    }
    let before = trace.len();
    trace.retain(|frame| !is_thread_run_with_frame(frame));
    before - trace.len()
}

impl<'a> NativeContextImpl<'a> {
    fn begin_native_region_lazy_inner(&mut self, band_lo: usize, band_hi: usize, require_locals_empty: bool) -> bool {
        // JIT round 16 wave 14 (lane ffm3): HotSpot's transition to
        // `_thread_in_native` -- one release store, no deposit, no TLAB
        // retire, no barrier lock. The thread stays a counted mutator; a
        // collection's take-over claims it while it is in C and scans it like
        // a peer frozen in compiled code (`gc_and_alloc::claim_lazy_native_peers`).
        // Declined (the caller falls back) where no take-over can claim it.
        if !lazy_native_claimable(self.shared)
            || !crate::jit::conservative_roots::current_thread_jit_chain_is_readable()
            || !crate::threading::jvm_thread::lazy_native_band_ok(band_lo, band_hi)
        {
            return false;
        }
        // JIT round 21 wave 19 (lane jnilazy): FFM downcalls decline while any
        // local frame holds a handle (`require_locals_empty == true`); JNI
        // calls (`require_locals_empty == false`, JNILAZY21R-1) enter lazy
        // with `locals_empty = false` so a claiming pause takes the non-precise
        // pinned claim and reads the published `JNI_LOCAL_FRAMES`.
        let locals_empty = crate::native::jni::local_ref_frames_are_empty();
        if require_locals_empty && !locals_empty {
            return false;
        }
        // Nothing in C logs a reference: drain what this thread logged, as
        // every deposit does, so a claimed thread holds no SATB residue.
        self.shared.mem.heap.flush_thread_satb();
        self.thread
            .gc_block_state
            .lazy_native_precise_claim
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.thread
            .gc_block_state
            .lazy_native_machine_roots_absent
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.thread
            .gc_block_state
            .lazy_native_deposit_eligible
            .store(
                self.shared.ffm_lazy_precise_claim_enabled
                    && cratonvm_jit::deopt::deopt_stashes_are_empty()
                    && locals_empty,
                std::sync::atomic::Ordering::Relaxed,
            );
        let cells = &self.thread.gc_block_state.native_excuse;
        if !cells.has_jit_cells() {
            publish_native_excuse_jit_cells(cells);
        }
        self.thread
            .gc_block_state
            .enter_lazy_native(band_lo, band_hi);
        true
    }
}
