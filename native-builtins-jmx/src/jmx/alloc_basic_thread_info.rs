#[allow(unused_imports)]
use super::*;

pub(in crate::jmx) fn alloc_basic_thread_info(
    ctx: &mut dyn NativeContext,
    thread_id: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    if thread_id <= 0 {
        // Match HotSpot's sun.management.ThreadImpl wording (id included) so
        // triage can see WHICH bad id a caller fed us.
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Invalid thread ID parameter: {thread_id}"),
        }
        .into());
    }

    let stack_element_cid = jmx_class_id_or_object(ctx, "java/lang/StackTraceElement");
    let monitor_info_cid = jmx_class_id_or_object(ctx, "java/lang/management/MonitorInfo");
    let lock_info_cid = jmx_class_id_or_object(ctx, "java/lang/management/LockInfo");

    let thread_name = ctx.create_string("main");
    let name_pin = ctx.pin_native_root(thread_name);
    let stack_trace = ctx.new_ref_array(stack_element_cid, 0);
    let stack_pin = ctx.pin_native_root(stack_trace);
    let locked_monitors = ctx.new_ref_array(monitor_info_cid, 0);
    let monitors_pin = ctx.pin_native_root(locked_monitors);
    let locked_synchronizers = ctx.new_ref_array(lock_info_cid, 0);
    let synchronizers_pin = ctx.pin_native_root(locked_synchronizers);

    let info = try_alloc_concurrent_synthetic(ctx, "java/lang/management/ThreadInfo", 18)?;

    let thread_name = ctx.read_native_pin(name_pin, thread_name);
    let stack_trace = ctx.read_native_pin(stack_pin, stack_trace);
    let locked_monitors = ctx.read_native_pin(monitors_pin, locked_monitors);
    let locked_synchronizers = ctx.read_native_pin(synchronizers_pin, locked_synchronizers);

    ctx.set_field_by_name(info, "threadName", Value::Object(Some(thread_name)));
    ctx.set_field_by_name(info, "threadId", Value::Long(thread_id));
    ctx.set_field_by_name(info, "blockedTime", Value::Long(-1));
    ctx.set_field_by_name(info, "blockedCount", Value::Long(0));
    ctx.set_field_by_name(info, "waitedTime", Value::Long(-1));
    ctx.set_field_by_name(info, "waitedCount", Value::Long(0));
    ctx.set_field_by_name(info, "lockOwnerId", Value::Long(-1));
    ctx.set_field_by_name(info, "priority", Value::Int(5));
    ctx.set_field_by_name(info, "stackTrace", Value::Object(Some(stack_trace)));
    ctx.set_field_by_name(info, "lockedMonitors", Value::Object(Some(locked_monitors)));
    ctx.set_field_by_name(
        info,
        "lockedSynchronizers",
        Value::Object(Some(locked_synchronizers)),
    );

    ctx.unpin_native_roots(name_pin);
    Ok(info)
}

/// Same shape as alloc_basic_thread_info, but with a caller-supplied real
/// thread name instead of the hardcoded "main" -- used by dumpAllThreads to
/// build one ThreadInfo per actually-enumerated live thread (Tomcat's
/// Diagnostics.getThreadDump() calls dumpAllThreads and greps the result for
/// connector I/O thread names like "http-nio-...").
pub(in crate::jmx) fn alloc_named_thread_info(
    ctx: &mut dyn NativeContext,
    thread_id: i64,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let stack_element_cid = jmx_class_id_or_object(ctx, "java/lang/StackTraceElement");
    let monitor_info_cid = jmx_class_id_or_object(ctx, "java/lang/management/MonitorInfo");
    let lock_info_cid = jmx_class_id_or_object(ctx, "java/lang/management/LockInfo");

    let thread_name = ctx.create_string(name);
    let name_pin = ctx.pin_native_root(thread_name);
    let stack_trace = ctx.new_ref_array(stack_element_cid, 0);
    let stack_pin = ctx.pin_native_root(stack_trace);
    let locked_monitors = ctx.new_ref_array(monitor_info_cid, 0);
    let monitors_pin = ctx.pin_native_root(locked_monitors);
    let locked_synchronizers = ctx.new_ref_array(lock_info_cid, 0);
    let synchronizers_pin = ctx.pin_native_root(locked_synchronizers);

    let info = try_alloc_concurrent_synthetic(ctx, "java/lang/management/ThreadInfo", 18)?;

    let thread_name = ctx.read_native_pin(name_pin, thread_name);
    let stack_trace = ctx.read_native_pin(stack_pin, stack_trace);
    let locked_monitors = ctx.read_native_pin(monitors_pin, locked_monitors);
    let locked_synchronizers = ctx.read_native_pin(synchronizers_pin, locked_synchronizers);

    ctx.set_field_by_name(info, "threadName", Value::Object(Some(thread_name)));
    ctx.set_field_by_name(info, "threadId", Value::Long(thread_id));
    ctx.set_field_by_name(info, "blockedTime", Value::Long(-1));
    ctx.set_field_by_name(info, "blockedCount", Value::Long(0));
    ctx.set_field_by_name(info, "waitedTime", Value::Long(-1));
    ctx.set_field_by_name(info, "waitedCount", Value::Long(0));
    ctx.set_field_by_name(info, "lockOwnerId", Value::Long(-1));
    ctx.set_field_by_name(info, "priority", Value::Int(5));
    ctx.set_field_by_name(info, "stackTrace", Value::Object(Some(stack_trace)));
    ctx.set_field_by_name(info, "lockedMonitors", Value::Object(Some(locked_monitors)));
    ctx.set_field_by_name(
        info,
        "lockedSynchronizers",
        Value::Object(Some(locked_synchronizers)),
    );

    ctx.unpin_native_roots(name_pin);
    Ok(info)
}

pub(in crate::jmx) fn alloc_jmx_lock_info(
    ctx: &mut dyn NativeContext,
    object: ObjectRef,
    class_name_override: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    let class_name = class_name_override
        .map(str::to_owned)
        .or_else(|| ctx.class_name_of_id(ctx.class_id_of_object(object)))
        .unwrap_or_else(|| "java/lang/Object".to_string())
        .class_text(&*ctx);
    // Read before anything allocates (gc-common w16-f): `object` is not
    // rooted here, and its identity hash was read off its address after both
    // allocations below.
    let identity = ctx.identity_hash_code(object);
    let class_name = ctx.create_string(&class_name);
    let class_pin = ctx.pin_native_root(class_name);
    let info = try_alloc_concurrent_synthetic(ctx, "java/lang/management/LockInfo", 2)?;
    let class_name = ctx.read_native_pin(class_pin, class_name);
    ctx.set_field_by_name(info, "className", Value::Object(Some(class_name)));
    ctx.set_field_by_name(info, "identityHashCode", Value::Int(identity));
    ctx.unpin_native_roots(class_pin);
    Ok(info)
}

pub(in crate::jmx) fn jmx_lock_name(
    ctx: &dyn NativeContext,
    object: ObjectRef,
    class_name_override: Option<&str>,
) -> String {
    let class_name = class_name_override
        .map(str::to_owned)
        .or_else(|| ctx.class_name_of_id(ctx.class_id_of_object(object)))
        .unwrap_or_else(|| "java/lang/Object".to_string())
        .class_text(&*ctx);
    format!("{class_name}@{:x}", ctx.identity_hash_code(object))
}

/// Build a `java.lang.management.MonitorInfo` for `object`, attributed to the
/// stack frame `locked_frame` at index `locked_depth`.
///
/// The two fields are `stackDepth` and `stackFrame` — NOT `lockedStackDepth` /
/// `lockedStackFrame`, which are the *getter* names (`getLockedStackDepth()` /
/// `getLockedStackFrame()`). Confirmed against JDK 25 with
/// `javap -p java.lang.management.MonitorInfo`:
///
/// ```text
/// private int stackDepth;
/// private java.lang.StackTraceElement stackFrame;
/// ```
///
/// Writing the getter names set nothing (`set_field_by_name` on an unknown name
/// is a silent no-op), so `getLockedStackFrame()` returned null and
/// `getLockedStackDepth()` returned the default 0 on *every* MonitorInfo this
/// VM ever produced. That pair violates the JDK's own invariant — depth >= 0
/// implies a non-null frame — and `org.apache.tomcat.util.Diagnostics
/// .getThreadDump` relies on it: it stores the monitor at
/// `monitorDepths[getLockedStackDepth()]` and then calls
/// `getLockedStackFrame().toString()`, so `GET /manager/text/threaddump`
/// answered 500 with an NPE (`TestManagerWebapp.testServlets`).
///
/// `className` / `identityHashCode` are inherited from `LockInfo` and *are*
/// spelled that way, which is why only these two were wrong.
pub(in crate::jmx) fn alloc_jmx_monitor_info(
    ctx: &mut dyn NativeContext,
    object: ObjectRef,
    locked_depth: i32,
    locked_frame: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    debug_assert_eq!(
        locked_depth >= 0,
        locked_frame.is_some(),
        "MonitorInfo depth/frame must agree: depth >= 0 iff a frame is attributed"
    );
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(object))
        .unwrap_or_else(|| "java/lang/Object".to_string())
        .class_text(&*ctx);
    // Before anything allocates (gc-common w16-f); see `alloc_jmx_lock_info`.
    // The frame is pinned before the class-name String too: it was held bare
    // across that allocation.
    let identity = ctx.identity_hash_code(object);
    let frame_pin = locked_frame.map(|f| (ctx.pin_native_root(f), f));
    let class_name = ctx.create_string(&class_name);
    let class_pin = ctx.pin_native_root(class_name);
    let info = try_alloc_concurrent_synthetic(ctx, "java/lang/management/MonitorInfo", 4)?;
    let info_pin = ctx.pin_native_root(info);
    let class_name = ctx.read_native_pin(class_pin, class_name);
    let info = ctx.read_native_pin(info_pin, info);
    ctx.set_field_by_name(info, "className", Value::Object(Some(class_name)));
    ctx.set_field_by_name(info, "identityHashCode", Value::Int(identity));
    // The interpreter exposes a complete frame trace but does not retain the
    // monitor-enter BCI, so every monitor a thread owns is attributed to its
    // innermost frame (depth 0). That is an approximation of *which* frame took
    // the lock, but it keeps the depth/frame pair internally consistent and
    // inside the stack trace's bounds, which is the part callers depend on.
    ctx.set_field_by_name(info, "stackDepth", Value::Int(locked_depth));
    let locked_frame = frame_pin.map(|(pin, f)| ctx.read_native_pin(pin, f));
    ctx.set_field_by_name(info, "stackFrame", Value::Object(locked_frame));
    // Everything from the lowest pin up (the frame's, when there is one).
    ctx.unpin_native_roots(frame_pin.map_or(class_pin, |(pin, _)| pin));
    Ok(info)
}

/// The snapshot `ThreadImpl.getThreadInfo1` reports: at most `max_depth`
/// innermost frames (all for a negative one), and no locked monitors or
/// synchronizers -- `getThreadInfo(id)` / `getThreadInfo(id, maxDepth)` do
/// not obtain them, only the `dumpThreads0` door does. HotSpot MEASURED
/// (`L6W60ThreadInfoMaxDepth.java`): depth 0 lists no
/// frame, depth 1 one, and none of them a monitor the thread holds.
/// Interpreter round i2 wave 60, lane L6: the native ignored both and listed
/// the whole stack with its monitors.
pub(in crate::jmx) fn thread_info1_snapshot(snapshot: ThreadJmxSnapshot, max_depth: i32) -> ThreadJmxSnapshot {
    limit_thread_info_snapshot(snapshot, false, false, max_depth)
}

/// The snapshot a `ThreadInfo` door reports for its arguments: at most
/// `max_depth` innermost frames (all for a negative one), the locked
/// monitors only when `locked_monitors` -- and then only those locked in a
/// frame that is kept -- and the locked synchronizers only when
/// `locked_synchronizers`. HotSpot MEASURED
/// (`L6W60ThreadInfoMaxDepth.java`):
/// `getThreadInfo(ids, false, false)` lists no monitor, and
/// `getThreadInfo(ids, true, true, 1)` of a thread holding a monitor two
/// frames down lists one frame and no monitor. Interpreter round i2 wave 60,
/// lane L6 (`dumpThreads0` ignored all three).
pub(in crate::jmx) fn limit_thread_info_snapshot(
    mut snapshot: ThreadJmxSnapshot,
    locked_monitors: bool,
    locked_synchronizers: bool,
    max_depth: i32,
) -> ThreadJmxSnapshot {
    if let Ok(keep) = usize::try_from(max_depth) {
        // Outermost first: the innermost `keep` frames are the tail.
        let drop = snapshot.stack_trace.len().saturating_sub(keep);
        snapshot.stack_trace.drain(..drop);
        // A monitor whose locking frame is cut goes with it; one the VM
        // could not attribute (no depths, or -1) stays at the innermost.
        let depths = std::mem::take(&mut snapshot.locked_monitor_depths);
        if depths.len() == snapshot.locked_monitors.len() {
            let keep_i32 = i32::try_from(keep).unwrap_or(i32::MAX);
            let (monitors, depths): (Vec<_>, Vec<_>) = snapshot
                .locked_monitors
                .iter()
                .copied()
                .zip(depths)
                .filter(|(_, d)| *d < keep_i32)
                .unzip();
            snapshot.locked_monitors = monitors;
            snapshot.locked_monitor_depths = depths;
        }
    }
    if !locked_monitors {
        snapshot.locked_monitors.clear();
        snapshot.locked_monitor_depths.clear();
    }
    if !locked_synchronizers {
        snapshot.locked_synchronizers.clear();
    }
    snapshot
}

/// Materialize the real JDK `ThreadInfo` layout from the VM's GC-safe lock
/// snapshot. This is intentionally shared by `ThreadMXBean` and
/// `sun.management.ThreadImpl`, which are two front doors to the same JMM
/// contract.
pub(in crate::jmx) fn alloc_snapshot_thread_info(
    ctx: &mut dyn NativeContext,
    snapshot: ThreadJmxSnapshot,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w16-f: ONE scope for the whole build, and the snapshot's own
    // objects rooted FIRST.
    //
    // The snapshot's thread, lock, monitors and synchronizers were pinned only
    // after the class initialisations (which can run `<clinit>`), the name
    // String and the stack-trace array -- so the pins could capture addresses
    // those had vacated. And the old tail released the thread's pin with
    // `unpin_native_roots(thread_pin)`, which truncates the pin stack: it also
    // released every monitor, synchronizer and ThreadInfo pin taken after it.
    // Everything after `Thread.getState()` (Java) then wrote through the
    // ThreadInfo, stack-trace and array addresses read BEFORE that call, and
    // the `lock` branch re-read `info_pin` -- a released slot the next pin
    // reused -- so its `lockName` store could land on the lock-name String.
    //
    // A thread parked in `LockSupport.park(blocker)` is not waiting on a monitor,
    // so the VM's snapshot carries no `lock` for it; HotSpot reports the
    // `parkBlocker` there (`ThreadInfo.getLockInfo()`: the `CountDownLatch$Sync` a
    // `CountDownLatch.await()` parks on, and so on), which Spring Boot's
    // `ThreadDumpEndpoint` prints as `- parking to wait for <..> (a ..)`.
    // Read before the scope exists: a field read allocates nothing.
    // `LockSupport.park(blocker)` sets the field before it parks and clears it
    // after, so a non-null `parkBlocker` on a thread the VM reports no monitor
    // for IS what the thread is parked on.
    let park_lock = if snapshot.lock.is_none() {
        snapshot.thread_object.and_then(|t| {
            match cratonvm_native_builtins::lang_misc::thread_field(ctx, t, "parkBlocker") {
                Value::Object(Some(blocker)) => Some(blocker),
                _ => None,
            }
        })
    } else {
        None
    };
    // Interpreter round i4 wave 84, lane L6: and the blocker's owner. HotSpot's
    // `ThreadSnapshot` takes `blocker_object_owner` from an
    // `AbstractOwnableSynchronizer` blocker's `exclusiveOwnerThread`, so a
    // thread parked in `ReentrantLock.lock()` reports the holder as
    // `getLockOwnerId()` / `getLockOwnerName()`; this left both at -1 / null.
    // Field reads only, before the scope.
    let mut snapshot = snapshot;
    if let (Some(blocker), Some(thread)) = (park_lock, snapshot.thread_object) {
        if let Some(owner_tid) = parked_synchronizer_owner_tid(ctx, thread) {
            let owner = ctx.get_field_by_name(blocker, "exclusiveOwnerThread");
            let owner_name = match owner {
                Value::Object(Some(o)) => match cratonvm_native_builtins::lang_misc::thread_field(ctx, o, "name") {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,                },
                _ => None,
            };
            snapshot.lock_owner_id = owner_tid;
            snapshot.lock_owner_name = owner_name;
        }
    }
    let mut scope = NativeHandleScope::new(ctx);
    let lock_h = snapshot.lock.or(park_lock).map(|o| scope.root(o));
    let thread_h = snapshot.thread_object.map(|o| scope.root(o));
    // A monitor is reported only when it can be attributed to a stack frame.
    // `lockedMonitors` means "monitors this thread locked *in a stack frame*",
    // and HotSpot gets that property structurally: its dumper discovers
    // monitors while walking Java frames, so a thread with no Java frames
    // reports none. We have no per-frame monitor map, so we mirror the
    // property directly — otherwise we would have to invent a depth for a
    // monitor with nowhere to put it, and `Diagnostics.getThreadDump` indexes
    // `new Object[stackTrace.length]` with exactly that depth.
    let attributable = !snapshot.stack_trace.is_empty();
    let reported_monitors: &[ObjectRef] = if attributable {
        &snapshot.locked_monitors
    } else {
        &[]
    };
    let monitor_hs: Vec<NativeHandle> = reported_monitors.iter().map(|&o| scope.root(o)).collect();
    let synchronizer_hs: Vec<NativeHandle> = snapshot
        .locked_synchronizers
        .iter()
        .map(|&o| scope.root(o))
        .collect();

    let _stack_element_cid = jmx_class_id_or_object(&mut *scope, "java/lang/StackTraceElement");
    let monitor_info_cid = jmx_class_id_or_object(&mut *scope, "java/lang/management/MonitorInfo");
    let lock_info_cid = jmx_class_id_or_object(&mut *scope, "java/lang/management/LockInfo");

    let thread_name = scope.create_string(&snapshot.thread_name);
    let name_h = scope.root(thread_name);
    let stack_trace =
        cratonvm_native_builtins::lang_system::build_stack_trace_element_array(&mut *scope, &snapshot.stack_trace)?;
    let stack_h = scope.root(stack_trace);
    let locked_monitors = scope.new_ref_array(monitor_info_cid, reported_monitors.len());
    let monitors_h = scope.root(locked_monitors);
    let locked_synchronizers =
        scope.new_ref_array(lock_info_cid, snapshot.locked_synchronizers.len());
    let synchronizers_h = scope.root(locked_synchronizers);

    for (i, h) in monitor_hs.iter().enumerate() {
        let monitor = scope.get(h);
        // Re-read frame 0 out of the rooted stack-trace array on every
        // iteration: the previous `alloc_jmx_monitor_info` allocated, so a
        // frame `ObjectRef` cached before the loop could have been relocated.
        // The array element is also the authority on whether a frame exists at
        // all — `attributable` says the snapshot had frames, but if the element
        // reads back null we must NOT claim depth 0 with no frame, which is the
        // exact inconsistent pair that NPE'd Tomcat's `Diagnostics`.
        //
        // Interpreter round i1 wave 24 (L7): the frame that locked the monitor
        // when the VM attributed it (`locked_monitor_depths`, from the frames'
        // own held-monitor records), else the innermost frame as before -- also
        // for a monitor held by no interpreted frame (`-1`: compiled code),
        // which keeps the depth/frame pair inside the trace for callers that
        // index by it.
        let stack_arr = scope.get(&stack_h);
        let stack_len = scope.array_length(stack_arr);
        let attributed = snapshot
            .locked_monitor_depths
            .get(i)
            .copied()
            .filter(|_| snapshot.locked_monitor_depths.len() == reported_monitors.len())
            .and_then(|d| usize::try_from(d).ok())
            .filter(|&d| d < stack_len);
        let frame_index = attributed.unwrap_or(0);
        let frame = if stack_len == 0 {
            None
        } else {
            match scope.get_array_element(stack_arr, frame_index) {
                Value::Object(Some(frame)) => Some(frame),
                _ => None,
            }
        };
        let depth = match frame {
            Some(_) => i32::try_from(frame_index).unwrap_or(-1),
            None => -1,
        };
        let frame = frame.filter(|_| depth >= 0);
        let info = alloc_jmx_monitor_info(&mut *scope, monitor, depth, frame)?;
        let arr = scope.get(&monitors_h);
        scope.set_array_element(arr, i, Value::Object(Some(info)));
    }
    for (i, h) in synchronizer_hs.iter().enumerate() {
        let synchronizer = scope.get(h);
        let info = alloc_jmx_lock_info(&mut *scope, synchronizer, None)?;
        let arr = scope.get(&synchronizers_h);
        scope.set_array_element(arr, i, Value::Object(Some(info)));
    }

    let info: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "java/lang/management/ThreadInfo", 18)?;
    let info_h = scope.root(info);
    // JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`,
    // with W6-F's held read): the state the VM read WITH the stack
    // (`ThreadJmxSnapshot::thread_state_at_trace`). A later
    // `Thread.getState()` of a thread looping through 1 ms parks answered
    // TIMED_WAITING over a stack sampled in `parkNanos`'s prologue, where
    // HotSpot reads both at one safepoint. The class's own constant, so `==`
    // holds; without a known state (or for a synthetic stand-in without the
    // constant), `Thread.getState()`, which is Java: it runs before any
    // address below is read.
    let at_trace = snapshot.thread_state_at_trace.and_then(|code| {
        let name = match code {
            1 => "RUNNABLE",
            2 => "TERMINATED",
            3 => "WAITING",
            4 => "BLOCKED",
            5 => "TIMED_WAITING",
            _ => return None,
        };
        cratonvm_native_builtins::lang_system::canonical_enum_constant(&mut *scope, "java/lang/Thread$State", name)    });
    let state = match (at_trace, thread_h.as_ref()) {
        (Some(state), _) => Some(state),
        (None, Some(h)) => {
            let thread = scope.get(&*h);
            match scope.invoke_virtual(thread, "getState", "()Ljava/lang/Thread$State;", &[]) {
                Ok(Some(state)) => Some(state),
                _ => None,
            }
        }
        (None, None) => None,
    };
    // Interpreter round i2 wave 59, lane L6: the thread's own priority and
    // daemon flag (`Thread.getPriority()` / `isDaemon()`, Java, so before any
    // address below is read). Both were constants -- `priority` 5, `daemon`
    // never written -- where HotSpot reports the thread's
    // (`L6W59ThreadInfoPriorityDaemon.java`).
    let (priority, daemon) = match thread_h.as_ref() {
        Some(h) => {
            let thread = scope.get(&*h);
            let priority = match scope.invoke_virtual(thread, "getPriority", "()I", &[]) {
                Ok(Some(Value::Int(p))) => Some(p),
                _ => None,
            };
            let thread = scope.get(&*h);
            let daemon = match scope.invoke_virtual(thread, "isDaemon", "()Z", &[]) {
                Ok(Some(Value::Int(d))) => Some(d != 0),
                _ => None,
            };
            (priority, daemon)        }
        None => (None, None),    };
    let info = scope.get(&info_h);
    if let Some(state) = state {
        scope.set_field_by_name(info, "threadState", state);
    }
    if daemon == Some(true) {
        scope.set_field_by_name(info, "daemon", Value::Int(1));
    }
    let thread_name = scope.get(&name_h);
    let stack_trace = scope.get(&stack_h);
    let locked_monitors = scope.get(&monitors_h);
    let locked_synchronizers = scope.get(&synchronizers_h);
    let contention_enabled = jmm_switch(&*scope, |s| s.thread_contention_monitoring);
    scope.set_field_by_name(info, "threadName", Value::Object(Some(thread_name)));
    scope.set_field_by_name(info, "threadId", Value::Long(snapshot.thread_id));
    scope.set_field_by_name(
        info,
        "blockedTime",
        Value::Long(if contention_enabled {
            snapshot.blocked_time_ms
        } else {
            -1
        }),
    );
    // THE COUNTS ARE NOT GATED, ONLY THE TIMES. `ThreadInfo.getBlockedTime()`
    // and `getWaitedTime()` are specified to return -1 while thread contention
    // monitoring is disabled; `getBlockedCount()` and `getWaitedCount()` carry
    // no such clause and are always available. Gating all four made
    // `getBlockedCount()` read 0 on a thread that was BLOCKED at that very
    // instant -- the case the number exists for -- while HotSpot 25 reports 1
    // on the same probe with contention monitoring left off
    // (`probes/JmxMonitorOwnership.java`, which is the oracle for this).
    scope.set_field_by_name(info, "blockedCount", Value::Long(snapshot.blocked_count));
    scope.set_field_by_name(
        info,
        "waitedTime",
        Value::Long(if contention_enabled {
            snapshot.waited_time_ms
        } else {
            -1
        }),
    );
    scope.set_field_by_name(info, "waitedCount", Value::Long(snapshot.waited_count));
    scope.set_field_by_name(info, "lockOwnerId", Value::Long(snapshot.lock_owner_id));
    scope.set_field_by_name(info, "priority", Value::Int(priority.unwrap_or(5)));
    // Interpreter round i2 wave 59, lane L6: `isInNative()` from JMM's
    // in-native flag, which the VM sets only under
    // `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE` (HotSpot MEASURED: true for a
    // thread blocked in `ServerSocket.accept`).
    if snapshot.thread_status & JMM_THREAD_STATE_FLAG_NATIVE != 0 {
        scope.set_field_by_name(info, "inNative", Value::Int(1));
    }
    scope.set_field_by_name(info, "stackTrace", Value::Object(Some(stack_trace)));
    scope.set_field_by_name(info, "lockedMonitors", Value::Object(Some(locked_monitors)));
    scope.set_field_by_name(
        info,
        "lockedSynchronizers",
        Value::Object(Some(locked_synchronizers)),
    );
    if let Some(lock_h) = lock_h.as_ref() {
        let lock = scope.get(lock_h);
        let lock_info =
            alloc_jmx_lock_info(&mut *scope, lock, snapshot.lock_class_name.as_deref())?;
        let info = scope.get(&info_h);
        scope.set_field_by_name(info, "lock", Value::Object(Some(lock_info)));
        let lock = scope.get(lock_h);
        let lock_name_text = jmx_lock_name(&*scope, lock, snapshot.lock_class_name.as_deref());
        let lock_name = scope.create_string(&lock_name_text);
        let info = scope.get(&info_h);
        scope.set_field_by_name(info, "lockName", Value::Object(Some(lock_name)));
    }
    if let Some(name) = snapshot.lock_owner_name.as_deref() {
        let name = scope.create_string(name);
        let info = scope.get(&info_h);
        scope.set_field_by_name(info, "lockOwnerName", Value::Object(Some(name)));
    }
    let info = scope.get(&info_h);
    Ok(info)
}

/// Populate the caller-supplied `long[] result` of
/// `sun.management.ThreadImpl.getThreadTotalCpuTime1(long[], long[])` (and its
/// `…UserCpuTime1` sibling) with per-id CPU or user time in nanoseconds.
///
/// STATIC native: `args[0]` is the id array and `args[1]` the result array. Both
/// are pinned across the loop because resolving a thread mirror can allocate.
/// Ids that name no live thread get the JMM's `-1`, which is also what the JDK
/// pre-fills the array with.
pub(in crate::jmx) fn fill_thread_cpu_times(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    user_time: bool,
) -> MethodCallResult {
    let ids = obj_arg(args, 0)?;
    let out = obj_arg(args, 1)?;
    let ids_pin = ctx.pin_native_root(ids);
    let out_pin = ctx.pin_native_root(out);
    let ids_live = ctx.read_native_pin(ids_pin, ids);
    let requested = read_long_array(ctx, ids_live);
    let len = requested.len().min(ctx.array_length(out));
    for (i, &thread_id) in requested.iter().take(len).enumerate() {
        let measured = if thread_id > 0 {
            cpu_time_for_requested_tid(ctx, Some(thread_id))
        } else {
            None
        };
        let value = measured.map_or(-1, |(cpu, user)| if user_time { user } else { cpu });
        let out_fresh = ctx.read_native_pin(out_pin, out);
        ctx.set_array_element(out_fresh, i, Value::Long(value));
    }
    ctx.unpin_native_roots(ids_pin);
    Ok(None)
}

/// Read a Java `long[]` argument into plain `i64`s.
///
/// Allocation-free, so `arr` cannot move underneath the walk; the caller then
/// holds ids rather than heap references, which is what makes the loops below
/// safe across the allocations they perform.
pub(in crate::jmx) fn read_long_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<i64> {
    (0..ctx.array_length(arr))
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Long(id) => id,
            Value::Int(id) => i64::from(id),
            _ => 0,
        })
        .collect()
}


/// The loop of `ThreadImpl.getThreadInfo1`: one `ThreadInfo` per id of the
/// pinned `ids` into the pinned `out`, at most `max_depth` frames each.
pub(in crate::jmx) fn thread_info1_fill(
    ctx: &mut dyn NativeContext,
    (ids_pin, ids): (usize, ObjectRef),
    (out_pin, out): (usize, ObjectRef),
    len: usize,
    max_depth: i32,
) -> MethodCallResult {
    for i in 0..len {
        let ids = ctx.read_native_pin(ids_pin, ids);
        let thread_id = match ctx.get_array_element(ids, i) {
            Value::Long(id) if id > 0 => id,
            Value::Int(id) if id > 0 => id as i64,
            _ => continue,
        };
        // Interpreter round i5 wave 100, lane L6: a virtual thread's id
        // leaves its slot null (`ThreadMXBean.getThreadInfo` answers
        // `null` for a virtual thread), as HotSpot finds no
        // `JavaThread` for it.
        if let Some(thread) = ctx.enumerate_threads(usize::MAX).into_iter().find(|thread| {
            matches!(cratonvm_native_builtins::lang_misc::thread_field(&*ctx, *thread, "tid"), Value::Long(id) if id == thread_id)
                && !is_virtual_thread_mirror(&*ctx, *thread)
        }) {
            // Interpreter round i2 wave 61, lane L6: a depth-0
            // `ThreadInfo` lists no frame, so none is captured.
            let snapshot = if max_depth == 0 {
                ctx.thread_jmx_snapshot_frameless(thread)
            } else {
                ctx.thread_jmx_snapshot(thread)
            };
            let info = snapshot
                .map(|snapshot| {
                    alloc_snapshot_thread_info(ctx, thread_info1_snapshot(snapshot, max_depth))
                })
                .unwrap_or_else(|| Ok(alloc_basic_thread_info(ctx, thread_id).expect("validated id")))?;
            let out = ctx.read_native_pin(out_pin, out);
            ctx.set_array_element(out, i, Value::Object(Some(info)));
        }
    }
    Ok(None)
}

/// One `ThreadInfo` per requested id, `null` where the id names no live thread
/// — the JMM's specified answer, shared by every
/// `ThreadMXBean.getThreadInfo(long[], …)` overload and by
/// `sun.management.ThreadImpl.dumpThreads0`.
///
/// Ids arrive as plain `i64` deliberately: each iteration allocates (the
/// `ThreadInfo`, its stack/monitor/synchronizer arrays, its name string), so a
/// held `Thread` reference would go stale under a moving GC. Each mirror is
/// re-resolved from its id inside the loop instead, and only the result array
/// is pinned.
pub(in crate::jmx) fn thread_info_array_for_ids(ctx: &mut dyn NativeContext, ids: &[i64]) -> MethodCallResult {
    thread_info_array_for_ids_limited(ctx, ids, (true, true, -1))
}

/// [`thread_info_array_for_ids`] with `(lockedMonitors, lockedSynchronizers,
/// maxDepth)` applied to each snapshot ([`limit_thread_info_snapshot`]).
pub(in crate::jmx) fn thread_info_array_for_ids_limited(
    ctx: &mut dyn NativeContext,
    ids: &[i64],
    (locked_monitors, locked_synchronizers, max_depth): (bool, bool, i32),
) -> MethodCallResult {
    let info_cid = jmx_class_id_or_object(ctx, "java/lang/management/ThreadInfo");
    let arr = ctx.new_ref_array(info_cid, ids.len());
    let arr_pin = ctx.pin_native_root(arr);
    // Interpreter round i2 wave 61, lane L6 (`CRATONVM_THREAD_DUMP_ONE_PAUSE`):
    // a dump that lists frames takes one frame-trace pause for all its
    // running targets (the references are read for it and not kept).
    let frameless = max_depth == 0 && !locked_monitors;
    if !frameless && ids.len() > 1 {
        let targets: Vec<_> = ids
            .iter()
            .filter(|&&id| id > 0)
            .filter_map(|&id| thread_object_for_java_tid(ctx, id))
            .collect();
        // JIT round 25 W10-7 review: and every target's `ThreadInfo` read
        // in that pause, not one more pause (up to three) per target.
        ctx.begin_thread_info_dump(&targets);
    }
    // nio21 wave 3, lane L4: one heap walk for the whole dump, as HotSpot's
    // `VM_ThreadDump` takes one `ConcurrentLocksDump`; its pins sit above
    // `arr_pin`, so the fill's final unpin releases them.
    let census = if locked_synchronizers {
        synchronizer_census(ctx)
    } else {
        None
    };
    let result = thread_info_array_fill(
        ctx,
        ids,
        arr_pin,
        arr,
        (locked_monitors, locked_synchronizers, max_depth),
        census.as_deref(),
    );
    ctx.end_thread_dump();
    result
}

/// The loop of [`thread_info_array_for_ids_limited`].
pub(in crate::jmx) fn thread_info_array_fill(
    ctx: &mut dyn NativeContext,
    ids: &[i64],
    arr_pin: usize,
    arr: ObjectRef,
    (locked_monitors, locked_synchronizers, max_depth): (bool, bool, i32),
    census: Option<&[(i64, usize, ObjectRef)]>,
) -> MethodCallResult {
    for (i, &thread_id) in ids.iter().enumerate() {
        // An unusable id leaves the slot null rather than aborting the whole
        // dump — the caller can still read every other thread.
        if thread_id <= 0 {
            continue;
        }
        // Wave 100 (lane L6): a virtual thread's id leaves its slot null, as
        // HotSpot's dump finds no `JavaThread` for it.
        let Some(thread) = platform_thread_object_for_java_tid(ctx, thread_id) else {
            continue;
        };
        // Interpreter round i2 wave 61, lane L6: a door that reports no frame
        // and no monitor captures no stack.
        let snapshot = if max_depth == 0 && !locked_monitors {
            ctx.thread_jmx_snapshot_frameless(thread)
        } else {
            ctx.thread_jmx_snapshot(thread)
        };
        let Some(mut snapshot) = snapshot else {
            continue;
        };
        if let Some(census) = census {
            snapshot.locked_synchronizers = census_synchronizers_of(&*ctx, census, thread_id);
        }
        let snapshot =
            limit_thread_info_snapshot(snapshot, locked_monitors, locked_synchronizers, max_depth);
        let info = alloc_snapshot_thread_info(ctx, snapshot)?;
        let arr_fresh = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr_fresh, i, Value::Object(Some(info)));
    }
    // Read the (possibly relocated) result before releasing the pin.
    let arr_final = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(arr_final))))
}

/// `ThreadMXBean.getThreadInfo(long[], …)` — instance form, so the id array is
/// `args[1]` (`args[0]` is the receiver). A null/absent array yields an empty
/// result: the JMM specifies NPE, but every in-tree caller reaches this through
/// a diagnostics path where an empty dump beats a crash.
pub(in crate::jmx) fn thread_info_array_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ids = match args.get(1) {
        Some(Value::Object(Some(arr))) => read_long_array(ctx, *arr),
        _ => Vec::new(),
    };
    thread_info_array_for_ids(ctx, &ids)}

/// Allocate a platform MXBean under its `com.sun.management` **extension**
/// interface rather than the `java.lang.management` base one.
///
/// On a real JVM `ManagementFactory.getThreadMXBean()` hands back an object
/// that implements `com.sun.management.ThreadMXBean` (HotSpot:
/// `com.sun.management.internal.HotSpotThreadImpl`), and
/// `getOperatingSystemMXBean()` one that implements
/// `com.sun.management.OperatingSystemMXBean`. CratonVM fabricated both as
/// instances of the *base* interface, so every
/// `instanceof com.sun.management.…` answered false — and a library that
/// feature-detects the extension (netty's chunk-reuse heuristic, and most
/// JVM-profiling libraries) silently took its fallback path with no exception
/// and no failing test to show for it.
///
/// Allocating under the extension name fixes the type test in both directions:
/// the extension interface *extends* the base one, so the base `instanceof`
/// and every `checkcast java/lang/management/…` keep working. In synthetic-JDK
/// mode the same relation comes from `class_manager::jdk_interfaces`, which
/// lists the base interface as the extension stub's supertype.
///
/// `base` is a genuine fallback, not a formality: a JDK image without the
/// `jdk.management` module has no extension interface to allocate against, and
/// the base bean is still the right answer there.
pub(in crate::jmx) fn alloc_extension_mxbean(
    ctx: &mut dyn NativeContext,
    extension: &str,
    base: &str,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    match try_alloc_concurrent_synthetic(ctx, extension, num_fields) {
        Ok(obj) => Ok(obj),
        Err(_) => try_alloc_concurrent_synthetic(ctx, base, num_fields),
    }
}

pub(crate) fn alloc_thread_mxbean(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = alloc_extension_mxbean(
        ctx,
        "com/sun/management/ThreadMXBean",
        "java/lang/management/ThreadMXBean",
        6,
    )?;
    init_thread_mxbean_fields(ctx, obj);
    Ok(obj)
}

/// Populate the 6 synthetic `ThreadMXBean` slots the count getters read by
/// index — shared by the factory path and the `<init>` native so both produce
/// the same bean.
pub(in crate::jmx) fn init_thread_mxbean_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    let thread_count = live_platform_thread_count(&*ctx);
    // daemonThreadCount (real): counted from `Thread.holder.daemon`, the same
    // flag the VM's own shutdown logic reads. Was a hard-coded 0.
    let daemon_count = daemon_thread_count(ctx);
    let peak_count = peak_thread_count(ctx);
    // Slots 4/5 are vestigial: `getCurrentThreadCpuTime` /
    // `getCurrentThreadUserTime` read the OS clock per call rather than a
    // per-bean snapshot (a cached CPU time would be wrong the instant the bean
    // was reused), so nothing reads them. Left at the JMM "unavailable"
    // sentinel.
    ctx.set_field(obj, 0, Value::Int(thread_count)); // threadCount (real)
    ctx.set_field(obj, 1, Value::Int(peak_count)); // peakThreadCount (high-water)
    ctx.set_field(obj, 2, Value::Long(thread_count as i64)); // totalStartedThreadCount (real)
    ctx.set_field(obj, 3, Value::Int(daemon_count)); // daemonThreadCount (real)
    ctx.set_field(obj, 4, Value::Long(-1)); // currentThreadCpuTime (unread)
    ctx.set_field(obj, 5, Value::Long(-1)); // currentThreadUserTime (unread)
}

pub(in crate::jmx) fn register_thread_mxbean(r: &mut NativeMethodRegistry) {
    // BOTH dispatch owners, for the same reason the
    // `setThreadContentionMonitoringEnabled` loop below already gives: the
    // receiver `getThreadMXBean()` hands back is now typed
    // `com.sun.management.ThreadMXBean` (see `alloc_extension_mxbean`), and an
    // `invokeinterface` resolving through that name must not fall through to
    // an abstract, Code-less entry. Registering the base name alone would turn
    // every call on the fixed bean into an AbstractMethodError — i.e. trading
    // a silent wrong answer for a loud crash.
    register_thread_mxbean_for("java/lang/management/ThreadMXBean", r);
    register_thread_mxbean_for("com/sun/management/ThreadMXBean", r);
    // Registered once, not per owner: this one already names its own owner
    // list, and repeating it would file three duplicate registrations.
    //
    // The public interface is abstract in the JDK image, while the concrete
    // implementation varies by release. Register all dispatch owners so an
    // invokeinterface does not fall through to an abstract Code-less entry.
    for owner in [
        "java/lang/management/ThreadMXBean",
        "com/sun/management/ThreadMXBean",
        "sun/management/ThreadImpl",
        "com/sun/management/internal/HotSpotThreadImpl",
    ] {
        r.register(
            owner,
            "setThreadContentionMonitoringEnabled",
            "(Z)V",
            native_set_thread_contention_monitoring_enabled, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    register_thread_mxbean_extensions(r);
    }

/// The `java.lang.management.ThreadMXBean` surface, registered under one
/// dispatch owner. Called once per owner — see [`register_thread_mxbean`].
pub(in crate::jmx) fn register_thread_mxbean_for(cls: &'static str, r: &mut NativeMethodRegistry) {
    // Interface — synthetic receivers only. Without this the count getters
    // below read untyped default slots instead of the live thread counts.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        init_thread_mxbean_fields(ctx, this);
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // REAL, and live: slot 0 is a snapshot `init_thread_mxbean_fields` takes
    // once, at `<init>`. `ManagementFactory.getThreadMXBean()` hands back one
    // cached bean, so reading the slot answered the thread count as of whenever
    // that bean was first built -- forever. This is the same defect
    // `getPeakThreadCount` below already names and fixes; it was fixed for one
    // of the four counters and left in the other three.
    r.register(cls, "getThreadCount", "()I", |ctx, _args| {
        // Wave 100 (lane L6): platform threads only, as `getLiveThreadCount`.
        Ok(Some(Value::Int(live_platform_thread_count(&*ctx))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // REAL: read the live process-wide high-water mark rather than slot 1's
    // construction-time snapshot — otherwise `resetPeakThreadCount()` is
    // invisible through this bean and the "peak" is frozen at whatever the
    // thread count happened to be when the bean was allocated.
    r.register(cls, "getPeakThreadCount", "()I", |ctx, _args| {
        Ok(Some(Value::Int(peak_thread_count(ctx))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // Live, for the reason `getThreadCount` above gives -- slot 2 is the same
    // `<init>` snapshot. NOT a monotone ever-started count, which is what the
    // JMM specifies and what neither registration of this triple ever
    // provided: nothing in the VM counts thread STARTS, only live threads, so
    // this can still go DOWN. Stated rather than papered over; a real counter
    // needs a hook at thread start, which is a larger change than converging
    // two bodies. `max(1)` because the calling thread is always one of them.
    // Interpreter round i5 wave 100, lane L6: the VM's started-thread
    // counter now exists (`total_started_thread_count`), so this is the
    // monotone JMM count, never below the live count.
    r.register(cls, "getTotalStartedThreadCount", "()J", |ctx, _args| {
        Ok(Some(Value::Long(total_started_thread_count(&*ctx).max(1))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // Live, same reason. `daemon_thread_count` is the helper
    // `init_thread_mxbean_fields` already calls to FILL slot 3 -- calling it
    // per query rather than once per bean is the whole change.
    r.register(cls, "getDaemonThreadCount", "()I", |ctx, _args| {
        Ok(Some(Value::Int(daemon_thread_count(ctx))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // REAL: the calling thread's CPU and user time, read from the OS
    // scheduler's own per-thread accounting (`current_thread_cpu_time_ns`).
    // The previous -1 was justified as the spec's "measurement is disabled"
    // sentinel, but the premise under it — "CratonVM has no per-thread CPU
    // accounting to wire in" — was never true for the CURRENT thread: the host
    // OS has been keeping that number all along and needs no VM bookkeeping to
    // hand it over. -1 survives only as the genuine fallback on a platform
    // whose per-thread clock we do not read.
    r.register(cls, "getCurrentThreadCpuTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(
            current_thread_cpu_time_ns().map_or(-1, |(cpu, _user)| cpu),
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getCurrentThreadUserTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(
            current_thread_cpu_time_ns().map_or(-1, |(_cpu, user)| user),
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // REAL for ANY id, not just the caller's. An id naming the CALLING thread
    // goes through the same clock as the two getters above (resolving the id
    // first is what makes the two surfaces agree); any other id now resolves the
    // Java `Thread` mirror to its OS tid via `NativeContext::thread_os_tid` and
    // reads that thread's platform clock — `OpenThread` + `GetThreadTimes` on
    // Windows, `/proc/self/task/<tid>/stat` fields 14/15 on Linux. -1 survives
    // as the JMM's genuine "not available": an unknown id, a thread with no OS
    // tid on record, one that has already exited, or a platform we do not read.
    //
    // Deliberately NOT converted to the spec's UnsupportedOperationException on
    // that fallback: Tomcat's Diagnostics formats these for every ThreadInfo in
    // a dump without guarding, and a throw would turn a working page into a 500.
    let thread_cpu_time: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = |ctx, args| {
        let requested = requested_thread_id(args);
        Ok(Some(Value::Long(
            cpu_time_for_requested_tid(ctx, requested).map_or(-1, |(cpu, _user)| cpu),
        )))
    };
    let thread_user_time: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = |ctx, args| {
        let requested = requested_thread_id(args);
        Ok(Some(Value::Long(
            cpu_time_for_requested_tid(ctx, requested).map_or(-1, |(_cpu, user)| user),
        )))
    };
    r.register(cls, "getThreadCpuTime", "(J)J", thread_cpu_time, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getThreadUserTime", "(J)J", thread_user_time, cratonvm_native_api::NativeKind::SyntheticStub);
    // REAL. This is specifically the ARBITRARY-thread question:
    // `isThreadCpuTimeSupported()` promises `getThreadCpuTime(id)` works for any
    // id. The escalated accessor now exists — `NativeContext::thread_os_tid`,
    // fed by `ThreadRegistry`'s per-thread `os_tid` (`set_os_tid_current`:
    // `GetCurrentThreadId()` on Windows, `SYS_gettid` on Linux) — so this is
    // answered by actually EXERCISING the arbitrary-thread route rather than by
    // asserting a platform capability: resolve the caller's own mirror to an OS
    // tid and read that tid through the same code path a foreign id takes. If
    // either step fails the answer stays `false`, which is still spec-legal
    // alongside a true `isCurrentThreadCpuTimeSupported()`.
    r.register(cls, "isThreadCpuTimeSupported", "()Z", |ctx, _args| {
        Ok(Some(Value::Int(i32::from(
            arbitrary_thread_cpu_time_supported(ctx),
        ))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // REAL: measurement is "enabled" exactly when the platform clock reads, so
    // the flag can never disagree with the numbers above. The old constant 0
    // said the current-thread getters were switched off while they were about
    // to hand back a real value.
    let cpu_time_available: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                jmm_switch(ctx, |s| s.thread_cpu_time) && current_thread_cpu_time_ns().is_some(),
            ))))
        };
    r.register(cls, "isThreadCpuTimeEnabled", "()Z", cpu_time_available, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        cls,
        "isCurrentThreadCpuTimeSupported",
        "()Z",
        |_ctx, _args| {
            // SUPPORT is a platform property and must NOT be gated on the
            // enable flag — a caller that disables measurement has not made
            // the feature unsupported, and `setThreadCpuTimeEnabled(false)`
            // followed by `isCurrentThreadCpuTimeSupported() == false` would
            // make re-enabling look impossible.
            Ok(Some(Value::Int(i32::from(
                current_thread_cpu_time_ns().is_some(),
            ))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // This has to exist now that `isThreadCpuTimeSupported()` can answer true:
    // the common library idiom is
    // `if (isThreadCpuTimeSupported()) setThreadCpuTimeEnabled(true)`
    // (Elasticsearch HotThreads, Netty, profilers), and an unregistered method
    // on the synthetic bean is an AbstractMethodError.
    //
    // IMPLEMENTED rather than no-op'd. The tempting justification — "the OS
    // scheduler's accounting is always running and cannot be switched off, so
    // enable is a no-op" — is true about the CLOCK and false about the API:
    // the JMM lets a caller DISABLE measurement, and after
    // `setThreadCpuTimeEnabled(false)` a no-op leaves `isThreadCpuTimeEnabled()`
    // still reporting true and the getters still handing back numbers. That is
    // a small lie, and the honest implementation is one stored flag rather than
    // anything the platform lacks — so this is an IMPLEMENT, not a
    // spec-correct constant. The CPU-time getters consult the flag and return
    // the JMM's `-1` sentinel while disabled.
    r.register(cls, "setThreadCpuTimeEnabled", "(Z)V", |ctx, args| {
        let enable = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        jmm_switch_set(ctx, |s| s.thread_cpu_time = enable);
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // TRUE on this (synthetic `ThreadMXBean` receiver) surface since
    // `6bb5b27eb` (2026-07-29). The comment that sat here said "KEEP false" and
    // described the gap that commit closed: ownership used to be published only
    // on the CONTENDED arm of `vm_exec::monitor_enter_blocking` /
    // `monitor_enter_synchronized_method`. Both now publish on the uncontended
    // (`None`) arm too, `invoke_fast`'s synchronized door does the same, and
    // `Object.wait()` publishes its waited-on monitor through
    // `set_jmx_waiting_monitor` / `take_jmx_waiting_monitor`.
    //
    // The real-JDK surface agrees since 2026-09-24: `sun.management.ThreadImpl`'s
    // bytecode answers from `VMManagementImpl.objectMonitorUsageSupport`, which
    // `initOptionalSupportFields` above now seeds `1`. Before that, the
    // 4-argument `getThreadInfo(ids, true, _, _)` and `dumpAllThreads(true, _)`
    // threw `UnsupportedOperationException` there (the 3-argument overload calls
    // `dumpThreads0` directly and never asked). Record:
    // `docs/feature-designs/native-dispatch-architecture.md`.
    r.register(
        cls,
        "isObjectMonitorUsageSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // REAL — VERIFIED 2026-07-28, and no longer the same question as the
    // monitor flag above. Ownable-synchronizer usage has no "fast path" to miss:
    // `AbstractOwnableSynchronizer.setExclusiveOwnerThread` is the JDK's single
    // authoritative ownership transition for every AQS-derived lock (acquire
    // passes the owner, release passes null), `register_thread_impl` below
    // intercepts it — `interpreter.rs:29149` forces that interception even for
    // JIT-compiled callers — and `NativeContext::record_jmx_owned_synchronizer`
    // now has a real VM override forwarding to
    // `ThreadRegistry::set_jmx_owned_synchronizer`, which re-homes the
    // synchronizer atomically so a hand-off cannot leave it recorded against two
    // threads. The read side is complete too: `thread_jmx_snapshot` returns
    // `locked_synchronizers` and `alloc_snapshot_thread_info` materialises a
    // `LockInfo` per entry, which `dumpAllThreads(ZZ)` and
    // `getThreadInfo([JZZ)` below both hand back. Non-exclusive synchronizers
    // (CountDownLatch, Semaphore, read locks) are absent because they
    // legitimately have no owner, which is what the JMM specifies.
    //
    // Gated, because the claim is only true while the REAL AQS is in use: under
    // `CRATONVM_SYNTHETIC_AQS`, `ReentrantLock.lock()` is itself a native that
    // never reaches `setExclusiveOwnerThread`, so the list would be silently
    // empty. See `management::synchronizer_usage_supported`.
    r.register(cls, "isSynchronizerUsageSupported", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(i32::from(synchronizer_usage_supported()))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // KEEP (the two below): false is the measurement, and CONFIRMED 2026-07-28
    // to be a different datum from the lock OWNERSHIP the two flags above turn
    // on — contention monitoring means TIMING every blocked and waiting interval
    // per thread, and this flag governs only that timing.
    //
    // AMENDED 2026-09-08. The paragraph here used to add that
    // "`ThreadJmxSnapshot` carries no blocked/waited time or count field at
    // all", and that is no longer true: the registry counts a block on the way
    // IN and `thread_jmx_snapshot` carries all four numbers. The counts were
    // nevertheless being zeroed alongside the times, which is a spec error, not
    // a missing source — `getBlockedTime()`/`getWaitedTime()` are specified to
    // return -1 while contention monitoring is disabled, and
    // `getBlockedCount()`/`getWaitedCount()` are specified with no such clause.
    // HotSpot 25 reports `getBlockedCount() == 1` for a thread blocked on a
    // monitor with contention monitoring left off; CratonVM reported 0 for the
    // same thread at the same instant. See `alloc_snapshot_thread_info` and
    // `probes/JmxMonitorOwnership.java`. This flag stays `false` — the TIMES
    // still have no source.
    // The JMM's answer for an unsupported optional feature is exactly `false`,
    // and a `false` from `...Supported()` makes
    // `setThreadContentionMonitoringEnabled` throw
    // `UnsupportedOperationException` from JDK bytecode — which is what keeps
    // the `...Enabled()` companion permanently false rather than it being an
    // independent guess. "Not enabled" is also the state HotSpot itself boots
    // in.
    // ES-FAIL-05 — Elasticsearch `HotThreads.initializeRuntimeMonitoring()` (run
    // from `ESTestCase.<clinit>`) calls `isThreadContentionMonitoringSupported()`;
    // it was unregistered on the synthetic ThreadMXBean → AbstractMethodError
    // ("no Code attribute"), failing ~every server unit test. Report `false`
    // (not supported): HotThreads then just logs "not supported" and returns,
    // never touching setThreadContentionMonitoringEnabled.
    // `setThreadContentionMonitoringEnabled` itself is registered once, by
    // `register_thread_mxbean`, because it names its own owner list.
    r.register(
        cls,
        "isThreadContentionMonitoringSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "isThreadContentionMonitoringEnabled",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(jmm_switch(ctx, |s| {
                s.thread_contention_monitoring
            })))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // REAL: every live thread's own `tid`. The previous body returned a
    // hard-coded `[1]` regardless of how many threads were running, and the
    // ids it handed out were not guaranteed to resolve: `getThreadInfo(long)`
    // below looks a thread up by matching that same `tid` field, so a
    // fabricated id yields a null ThreadInfo. Enumerating for real makes the
    // pair consistent — every id returned here resolves.
    r.register(cls, "getAllThreadIds", "()[J", |ctx, _args| {
        use cratonvm_types::ArrayElementType;
        // Ids are collected as plain i64 first: `new_array` can GC, and the
        // thread references backing them are not pinned.
        let ids = live_thread_ids(ctx);
        let arr = ctx.new_array(ArrayElementType::Long, ids.len());
        for (i, id) in ids.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Long(*id));
        }
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        cls,
        "getThreadInfo",
        "(J)Ljava/lang/management/ThreadInfo;",
        |ctx, args| {
            let thread_id = args
                .iter()
                .find_map(|arg| match arg {
                    Value::Long(id) => Some(*id),
                    Value::Int(id) => Some(*id as i64),
                    _ => None,
                })
                .unwrap_or_else(|| ctx.thread_id().max(1) as i64);
            if thread_id <= 0 {
                // Same wording as HotSpot's sun.management.ThreadImpl
                // verifier, id included, so triage can see WHICH bad id a
                // caller fed us.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("Invalid thread ID parameter: {thread_id}"),
                }
                .into());
            }
            let thread = ctx.enumerate_threads(usize::MAX).into_iter().find(|thread| {
                matches!(cratonvm_native_builtins::lang_misc::thread_field(&*ctx, *thread, "tid"), Value::Long(id) if id == thread_id)
            });
            let info = match thread.and_then(|thread| ctx.thread_jmx_snapshot(thread)) {
                Some(snapshot) => Some(alloc_snapshot_thread_info(ctx, snapshot)?),
                None => None,
            };
            Ok(Some(Value::Object(info)))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // REAL: one ThreadInfo per requested id. All four overloads share
    // `thread_info_array_for_ids`; the extra `maxDepth` / `lockedMonitors` /
    // `lockedSynchronizers` parameters do not change WHICH threads are reported
    // and the snapshot already carries the full stack plus both lock lists.
    //
    // The previous empty array was defensible only as an NPE dodge for Surefire
    // ForkedBooter.generateThreadDump (its for-loop then iterated zero times
    // instead of failing on `arraylength` of null) — but it is a fabricated "no
    // such threads" answer for ids the caller just got from `getAllThreadIds()`,
    // and it is now load-bearing: `isSynchronizerUsageSupported()` above reports
    // true, and the JDK contract a client checks that flag for is precisely
    // `getThreadInfo(ids, false, true)` returning populated
    // `lockedSynchronizers`. JBoss/Quarkus diagnostics use the same forms.
    let thread_info_for_ids: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        thread_info_array_native;
    for desc in [
        "([JI)[Ljava/lang/management/ThreadInfo;",
        "([J)[Ljava/lang/management/ThreadInfo;",
        "([JZZ)[Ljava/lang/management/ThreadInfo;",
        "([JZZI)[Ljava/lang/management/ThreadInfo;",
    ] {
        r.register(cls, "getThreadInfo", desc, thread_info_for_ids, cratonvm_native_api::NativeKind::SyntheticStub);
    }
    // REAL(detector): the wait-for-graph walk the old comment here described as
    // "a separate piece of work" now exists — `deadlocked_thread_ids` — and
    // these share it with `sun.management.ThreadImpl`'s `*0` natives so all
    // four surfaces agree. `null` still means "no deadlocked threads" (these
    // methods return null, not an empty array), so null-checking callers are
    // unaffected; what changed is that the null is now a checked result.
    // `deadlocked_thread_ids` documents the one remaining gap: the VM builds
    // its per-thread snapshot without lock-owner fields, so the graph is empty
    // until that is plumbed through.
    r.register(cls, "findDeadlockedThreads", "()[J", |ctx, _args| {
        deadlocked_threads_result(ctx, true)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "findMonitorDeadlockedThreads", "()[J", |ctx, _args| {
        deadlocked_threads_result(ctx, false)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // dumpAllThreads(boolean, boolean) -- Tomcat's Diagnostics.getThreadDump()
    // (manager threaddump command) calls this and greps the result for
    // connector I/O thread names (e.g. "http-nio-..."). Build one
    // (name-only) ThreadInfo per actually-live thread via enumerate_threads
    // rather than returning an empty array -- lock/monitor/synchronizer
    // details (the two boolean params) are out of scope, matching the
    // "basic" ThreadInfo helper's existing level of fidelity.
    r.register(
        cls,
        "dumpAllThreads",
        "(ZZ)[Ljava/lang/management/ThreadInfo;",
        |ctx, _args| {
            // Round 15 wave 5 (lane threadvm2; `r15w4-threadvm-review-findings`
            // F3): every live thread, as HotSpot answers and as
            // `getAllThreadIds` / `getThreadInfo` enumerate. It stopped at 256,
            // below a Tomcat server's own thread count.
            let threads = ctx.enumerate_threads(usize::MAX);
            // Each iteration allocates (thread name string, ThreadInfo's own
            // arrays, the ThreadInfo itself) and calls `getName()` (Java), so a
            // moving GC can run mid-loop.
            //
            // gc-common w19-e: every thread is rooted UP FRONT. They used to
            // be pinned one at a time as the loop reached them -- pinning an
            // address the earlier iterations' allocations (and the
            // `ThreadInfo` class initialisation and the array's allocation
            // before the loop) had already vacated -- and the snapshot was
            // taken through the address from before `getName()`. The per-
            // iteration pins were never released either.
            let mut scope = NativeHandleScope::new(ctx);
            let thread_hs: Vec<NativeHandle> = threads.iter().map(|&t| scope.root(t)).collect();
            // JIT round 25 W10-7 review: every thread's `ThreadInfo` read in
            // the dump's own pauses (`begin_thread_info_dump`), as HotSpot's
            // one safepoint; `end_thread_dump` on every way out below.
            let refs: Vec<ObjectRef> = thread_hs.iter().map(|h| scope.get(h)).collect();
            scope.begin_thread_info_dump(&refs);
            // nio21 wave 3, lane L4: this door has always reported the
            // snapshot's synchronizers; with the per-lock index off they come
            // from one dump-time heap walk, rooted in this scope at once.
            let census: Option<Vec<(i64, NativeHandle)>> = synchronizer_census_rows(&mut *scope)
                .map(|rows| {
                    rows.into_iter()
                        .map(|(tid, synchronizer)| (tid, scope.root(synchronizer)))
                        .collect()
                });
            let info_cid = jmx_class_id_or_object(&mut *scope, "java/lang/management/ThreadInfo");
            let n = thread_hs.len();
            let arr = scope.new_ref_array(info_cid, n);
            let arr_h = scope.root(arr);
            for (i, t_h) in thread_hs.into_iter().enumerate() {
                let t = scope.get(&t_h);
                let name = scope
                    .invoke_virtual(t, "getName", "()Ljava/lang/String;", &[])
                    .ok()
                    .flatten()
                    .and_then(|v| match v {
                        Value::Object(Some(s)) => scope.read_string(s),
                        _ => None,
                    })
                    .unwrap_or_else(|| format!("Thread-{i}"));
                let t = scope.get(&t_h);
                let mut info = scope
                    .thread_jmx_snapshot(t)
                    .unwrap_or_else(|| ThreadJmxSnapshot {
                        thread_id: scope.thread_id() as i64,
                        thread_name: name,
                        ..ThreadJmxSnapshot::default()
                    });
                if let Some(census) = census.as_deref() {
                    let thread_id = info.thread_id;
                    info.locked_synchronizers = census
                        .iter()
                        .filter(|(tid, _)| *tid == thread_id)
                        .map(|(_, h)| scope.get(h))
                        .collect();
                }
                let info = match alloc_snapshot_thread_info(&mut *scope, info) {
                    Ok(info) => info,
                    Err(e) => {
                        scope.end_thread_dump();
                        return Err(e);
                    }
                };
                let arr = scope.get(&arr_h);
                scope.set_array_element(arr, i, Value::Object(Some(info)));
            }
            scope.end_thread_dump();
            let arr = scope.get(&arr_h);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

/// The methods `com.sun.management.ThreadMXBean` adds on top of the base
/// interface, registered under the extension name **only** — they do not exist
/// on `java.lang.management.ThreadMXBean` and registering them there would
/// claim a surface the base interface does not have.
///
/// The allocation counters are real, not sentinels:
/// [`NativeContext::current_thread_allocated_bytes`] reads
/// `Tlab::thread_allocated_bytes`, which is derived from the live TLAB cursor
/// plus the recorded non-TLAB allocations — so it sees compiled code's inline
/// bump as well as the interpreter's, and humongous objects that skipped the
/// TLAB entirely. `-1` survives only as the JMM's genuine "not available",
/// which is the honest answer for a context with no thread accounting at all.
///
/// **Only the CALLING thread is measurable.** The counter lives in the
/// thread's own TLAB, which no other thread may read while its owner is
/// running. `getThreadAllocatedBytes(id)` therefore answers for the caller's
/// own id (and for the JDK's `0` == "current thread" convention) and returns
/// the JMM's `-1` for any other id — the documented "value not available",
/// which callers already handle. Claiming a number for a foreign thread would
/// mean either a fabricated figure or a data race on a live cursor.
pub(in crate::jmx) fn register_thread_mxbean_extensions(r: &mut NativeMethodRegistry) {
    let cls = "com/sun/management/ThreadMXBean";

    /// Bytes allocated by the caller, or `None` when this VM cannot account
    /// for the requested thread.
    fn allocated_bytes_for(ctx: &mut dyn NativeContext, requested: Option<i64>) -> Option<u64> {
        // `0` is the JDK's own spelling of "the current thread"
        // (`getCurrentThreadAllocatedBytes()` compiles to a `0L` id), the same
        // convention `cpu_time_for_requested_tid` follows.
        let requested = requested.filter(|id| *id != 0);
        if !current_thread_tid_matches(ctx, requested) {
            return None;
        }
        ctx.current_thread_allocated_bytes()
    }

    let allocated_scalar: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = |ctx, args| {
        let requested = requested_thread_id(args);
        Ok(Some(Value::Long(
            allocated_bytes_for(ctx, requested)
                .map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX)),
        )))
    };
    r.register(cls, "getThreadAllocatedBytes", "(J)J", allocated_scalar, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        cls,
        "getCurrentThreadAllocatedBytes",
        "()J",
        |ctx, _args| {
            Ok(Some(Value::Long(
                ctx.current_thread_allocated_bytes()
                    .map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX)),
            )))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // Array form: one entry per requested id, `-1` where the id is not the
    // caller's. Sized from the input array, as the JMM specifies.
    r.register(cls, "getThreadAllocatedBytes", "([J)[J", |ctx, args| {
        use cratonvm_types::ArrayElementType;
        let ids = match args.get(1) {
            Some(Value::Object(Some(arr))) => read_long_array(ctx, *arr),
            _ => Vec::new(),
        };
        let answers: Vec<i64> = ids
            .iter()
            .map(|id| {
                allocated_bytes_for(ctx, Some(*id))
                    .map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX))
            })
            .collect();
        let out = ctx.new_array(ArrayElementType::Long, answers.len());
        for (i, v) in answers.iter().enumerate() {
            ctx.set_array_element(out, i, Value::Long(*v));
        }
        Ok(Some(Value::Object(Some(out))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // SUPPORT is a platform property, not a per-call outcome: it answers
    // whether this VM keeps the accounting at all, which it does exactly when
    // the current thread can be read. Gating it on the *enable* flag would
    // make re-enabling look impossible — the same reasoning
    // `isCurrentThreadCpuTimeSupported` gives above.
    r.register(
        cls,
        "isThreadAllocatedMemorySupported",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                ctx.current_thread_allocated_bytes().is_some(),
            ))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "isThreadAllocatedMemoryEnabled",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                jmm_switch(ctx, |s| s.thread_allocated_memory)
                    && ctx.current_thread_allocated_bytes().is_some(),
            ))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // Stored, not no-op'd — same argument as `setThreadCpuTimeEnabled`: the
    // underlying accounting cannot be switched off, but the JMM lets a caller
    // disable *measurement*, and a no-op would leave `isEnabled()` reporting
    // true right after a `setEnabled(false)`.
    r.register(
        cls,
        "setThreadAllocatedMemoryEnabled",
        "(Z)V",
        |ctx, args| {
            let enable = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
            jmm_switch_set(ctx, |s| s.thread_allocated_memory = enable);
            Ok(None)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // `getTotalThreadAllocatedBytes()` is process-wide and CUMULATIVE — a
    // total over the life of the process that never decreases.
    //
    // This used to answer with `heap_allocated_bytes()`, which is not that. It
    // is an occupancy gauge derived from `used - free`, so it FALLS at every
    // collection, and a caller measuring a window containing a GC gets the
    // difference of two occupancies. Hibernate's `HqlParserMemoryUsageTest`
    // measures exactly such a window, and read the identical HQL parse as
    // 488 MB under ZGC, 49 MB under Generational at a 2 GB heap, and 458 MB
    // under Generational at 8 GB — same bytecode, three answers.
    //
    // `NativeContext::total_allocated_bytes` is the real thing: this VM's
    // `HeapRealm::thread_allocated_total`, fed from every TLAB retire and (in
    // 64 KiB batches) every non-TLAB allocation, so it is the sum of all of
    // this VM's threads' published totals. The calling thread's live TLAB span
    // and held-back carry are added on top because they are the one in-flight
    // amount readable here -- other threads' cursors may not be read while
    // their owner runs, which bounds the under-count at about one TLAB per
    // running thread and keeps the answer monotonic.
    r.register(cls, "getTotalThreadAllocatedBytes", "()J", |ctx, _args| {
        let total = ctx
            .total_allocated_bytes()
            .unwrap_or_else(|| ctx.heap_allocated_bytes() as u64);
        Ok(Some(Value::Long(i64::try_from(total).unwrap_or(i64::MAX))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // The bulk CPU/user-time forms the extension interface adds. Same
    // per-id resolution the scalar `getThreadCpuTime(J)` uses, so the two
    // surfaces cannot disagree.
    let cpu_time_array: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |ctx, args| thread_time_array(ctx, args, true);
    let user_time_array: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |ctx, args| thread_time_array(ctx, args, false);
    r.register(cls, "getThreadCpuTime", "([J)[J", cpu_time_array, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getThreadUserTime", "([J)[J", user_time_array, cratonvm_native_api::NativeKind::SyntheticStub);
}

/// Shared body of `com.sun.management.ThreadMXBean.getThreadCpuTime([J)` and
/// `getThreadUserTime([J)`: one answer per requested id, `-1` where the JMM's
/// "not available" applies.
pub(in crate::jmx) fn thread_time_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    want_cpu: bool,
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let ids = match args.get(1) {
        Some(Value::Object(Some(arr))) => read_long_array(ctx, *arr),
        _ => Vec::new(),
    };
    let answers: Vec<i64> =
        ids.iter()
            .map(|id| {
                cpu_time_for_requested_tid(ctx, Some(*id)).map_or(-1, |(cpu, user)| {
                    if want_cpu {
                        cpu
                    } else {
                        user
                    }
                })
            })
            .collect();
    let out = ctx.new_array(ArrayElementType::Long, answers.len());
    for (i, v) in answers.iter().enumerate() {
        ctx.set_array_element(out, i, Value::Long(*v));
    }
    Ok(Some(Value::Object(Some(out))))
}

// ---------------------------------------------------------------------------
// 6. ClassLoadingMXBean — 3-field synthetic
// ---------------------------------------------------------------------------

/// Slot holding the `verbose` flag `setVerbose`/`isVerbose` round-trip
/// through. Slots 0..2 are the three class-count metrics.
pub(in crate::jmx) const CLM_VERBOSE: usize = 3;

pub(in crate::jmx) fn alloc_class_loading_mxbean(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/management/ClassLoadingMXBean", 4)?;
    init_class_loading_mxbean_fields(ctx, obj);
    Ok(obj)
}

/// Populate the synthetic `ClassLoadingMXBean` slots — shared by the factory
/// path and the `<init>` native.
pub(in crate::jmx) fn init_class_loading_mxbean_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    let loaded = ctx.loaded_class_count() as i32;
    ctx.set_field(obj, 0, Value::Int(loaded)); // loadedClassCount (real)
    ctx.set_field(
        obj,
        1,
        Value::Long(loaded as i64 + ctx.unloaded_class_count() as i64),
    );
    ctx.set_field(obj, 2, Value::Long(ctx.unloaded_class_count() as i64));
    // Verbose class loading starts off, exactly as it does on a HotSpot that
    // was not given `-verbose:class`.
    ctx.set_field(obj, CLM_VERBOSE, Value::Int(0));
}

pub(in crate::jmx) fn register_class_loading_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/ClassLoadingMXBean";
    // Interface — synthetic receivers only. Establishes the verbose flag the
    // setter/getter pair below round-trip through.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        init_class_loading_mxbean_fields(ctx, this);
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(cls, "getLoadedClassCount", "()I", |ctx, _args| {
        Ok(Some(Value::Int(ctx.loaded_class_count() as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getTotalLoadedClassCount", "()J", |ctx, _args| {
        Ok(Some(Value::Long(
            ctx.loaded_class_count() as i64 + ctx.unloaded_class_count() as i64,
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getUnloadedClassCount", "()J", |ctx, _args| {
        Ok(Some(Value::Long(ctx.unloaded_class_count() as i64)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // isVerbose/setVerbose must round-trip: the JMM contract is that
    // `setVerbose(b)` is observable through `isVerbose()`. Previously the
    // setter discarded the flag and the getter answered a hard-coded `false`,
    // so `setVerbose(!isVerbose())` left the bean unchanged — a live,
    // measured defect. CratonVM has no `-verbose:class` tracing to actually
    // switch on, so the flag is state-only; that is a smaller lie than
    // dropping the caller's write entirely, and it is what a HotSpot with
    // class-load logging disabled reports too.
    r.register(cls, "isVerbose", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let verbose = match ctx.get_field(this, CLM_VERBOSE) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(verbose)))    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "setVerbose", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let verbose = match args.get(1) {
            Some(Value::Int(v)) => i32::from(*v != 0),
            _ => 0,
        };
        ctx.set_field(this, CLM_VERBOSE, Value::Int(verbose));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// 7. OperatingSystemMXBean — 5-field synthetic
// ---------------------------------------------------------------------------

pub(crate) fn alloc_os_mxbean(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    // See `alloc_extension_mxbean`: `com.sun.management.OperatingSystemMXBean`
    // is the standard source of process/system CPU load and physical-memory
    // figures, so the same silent-fallback risk applied here as on the thread
    // bean.
    let obj = alloc_extension_mxbean(
        ctx,
        "com/sun/management/OperatingSystemMXBean",
        "java/lang/management/OperatingSystemMXBean",
        5,
    )?;
    let mut obj = obj;
    init_os_mxbean_fields(ctx, &mut obj);
    Ok(obj)
}

/// Populate the 5 synthetic `OperatingSystemMXBean` slots — shared by the
/// factory path and the `<init>` native.
pub(in crate::jmx) fn init_os_mxbean_fields(ctx: &mut dyn NativeContext, obj: &mut ObjectRef) {
    // GC: three `create_string` calls, each followed by a store THROUGH `obj`.
    // The receiver is `&mut` so the caller's copy is corrected too, and every
    // unconverted caller is a compile error — the remedy this file's gate
    // prescribes (`WORKER-5-NOTE-10` §7.3). The audit only started reporting
    // this once `ctx.create_string` was added to its level-0 set; it had been
    // missing while three tokens that match nothing in the tree were present.
    let pin = ctx.pin_native_root(*obj);
    // REAL: OS name / arch / version come from the live process, and all three
    // come from the SAME place the corresponding system property does — the
    // `os.name` / `os.arch` / `os.version` properties the VM seeds at startup
    // (vm_init's `canonical_os_name` / `canonical_os_arch` /
    // `canonical_os_version`, which reproduce HotSpot's spellings).
    //
    // `getName`/`getArch`/`getVersion` are SPECIFIED as those properties, so
    // reading anything else is a divergence by construction. `std::env::consts`
    // is exactly such an "anything else": it answers `"windows"` where HotSpot
    // says `"Windows 11"`, and `"x86_64"` where HotSpot says `"amd64"` — the
    // bean and `System.getProperty("os.name")` then disagree inside one VM.
    // The consts survive only as the fallback for a VM whose property table is
    // somehow unseeded, where a lowercase-but-true answer beats "unknown".
    let os_name = ctx
        .get_system_property("os.name")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from(std::env::consts::OS));
    let name = ctx.create_string(&os_name);
    *obj = ctx.read_native_pin(pin, *obj);
    ctx.set_field(*obj, 0, Value::Object(Some(name)));
    let os_arch = ctx
        .get_system_property("os.arch")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from(std::env::consts::ARCH));
    let arch = ctx.create_string(&os_arch);
    *obj = ctx.read_native_pin(pin, *obj);
    ctx.set_field(*obj, 1, Value::Object(Some(arch)));
    let os_version = ctx
        .get_system_property("os.version")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("unknown"));
    let version = ctx.create_string(&os_version);
    *obj = ctx.read_native_pin(pin, *obj);
    ctx.set_field(*obj, 2, Value::Object(Some(version)));
    // Container-aware processor count (cgroup CPU quota under container support).
    let cpus = ctx.available_processor_count();
    ctx.set_field(*obj, 3, Value::Int(cpus));
    // Slot 4 = system load average. `getSystemLoadAverage()` answers live
    // rather than from this slot, but seed it with the same real value so a
    // direct slot read is not the only place still reporting -1.0.
    ctx.set_field(*obj, 4, Value::Double(system_load_average()));
    ctx.unpin_native_roots(pin);
}

pub(in crate::jmx) fn register_operating_system_mxbean(r: &mut NativeMethodRegistry) {
    // Both dispatch owners — `getOperatingSystemMXBean()` now hands back a
    // receiver typed `com.sun.management.OperatingSystemMXBean` (see
    // `alloc_extension_mxbean`), and an `invokeinterface` resolving through
    // that name must find a body rather than an abstract, Code-less entry.
    for cls in [
        "java/lang/management/OperatingSystemMXBean",
        "com/sun/management/OperatingSystemMXBean",
    ] {
        // Interface — synthetic receivers only. All four index-based getters
        // below would otherwise read null / untyped default slots.
        r.register(cls, "<init>", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mut this = this;
            init_os_mxbean_fields(ctx, &mut this);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge);

        r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(cls, "getArch", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(cls, "getVersion", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(cls, "getAvailableProcessors", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        }, cratonvm_native_api::NativeKind::Bridge);
        // REAL where the platform publishes it: Linux's `/proc/loadavg`. The
        // flat -1.0 was a fabricated "unavailable" on the one platform that
        // does have the number (and the sibling registration in
        // `phases_late::management` already read it). Non-Linux keeps the
        // spec's -1.0 sentinel.
        r.register(cls, "getSystemLoadAverage", "()D", |_ctx, _args| {
            Ok(Some(Value::Double(system_load_average())))
        }, cratonvm_native_api::NativeKind::Bridge);
    }
    register_operating_system_mxbean_extensions(r);
    }

/// The methods `com.sun.management.OperatingSystemMXBean` adds on top of the
/// base interface, registered under the extension name only.
///
/// Same single source as the `*0` natives (`os_metrics`), so the interface and
/// the JDK's own `OperatingSystemImpl` cannot answer differently for the same
/// metric on the same VM. Both the JDK 25 spellings and the deprecated JDK 8
/// ones are registered: the deprecated pair is still what a great deal of
/// library code calls.
pub(in crate::jmx) fn register_operating_system_mxbean_extensions(r: &mut NativeMethodRegistry) {
    let cls = "com/sun/management/OperatingSystemMXBean";
    for (name, metric) in os_metric_long_natives() {
        // The interface methods are the `*0` names without the trailing `0`.
        let public = name.strip_suffix('0').unwrap_or(name);
        r.register(cls, public, "()J", metric, cratonvm_native_api::NativeKind::SyntheticStub);
    }
    for (name, metric) in os_metric_long_natives_jdk9() {
        let public = name.strip_suffix('0').unwrap_or(name);
        // `os_metric_long_natives_jdk9` renames only two entries; the rest
        // would be duplicates of the loop above.
        if matches!(public, "getFreeMemorySize" | "getTotalMemorySize") {
            r.register(cls, public, "()J", metric, cratonvm_native_api::NativeKind::SyntheticStub);
        }
    }
    // See `register_operating_system_impl`: a CPU load needs a previous
    // sample, so -1.0 is the measurement, not a placeholder.
    let neg_one_double: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Double(-1.0)));
    for name in ["getCpuLoad", "getSystemCpuLoad", "getProcessCpuLoad"] {
        r.register(cls, name, "()D", neg_one_double, cratonvm_native_api::NativeKind::SyntheticStub);
    }
}

// ---------------------------------------------------------------------------
// 8. CompilationMXBean — 3-field synthetic
// ---------------------------------------------------------------------------

pub(in crate::jmx) fn alloc_compilation_mxbean(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/management/CompilationMXBean", 3)?;
    let mut obj = obj;
    init_compilation_mxbean_fields(ctx, &mut obj);
    Ok(obj)
}

/// Populate the 3 synthetic `CompilationMXBean` slots — shared by the factory
/// path and the `<init>` native.
pub(in crate::jmx) fn init_compilation_mxbean_fields(ctx: &mut dyn NativeContext, obj: &mut ObjectRef) {
    // name = CratonVM's real JIT identity (not a fabricated foreign name).
    // REAL: slots 1 and 2 are seeded from the ONE accessor
    // `NativeContext::jit_total_compile_time_ms`, so a bean can never be built
    // claiming support for a number that is not kept. `None` (JIT disabled)
    // keeps the old 0 / false. The getters below re-read live rather than
    // trusting these slots — a compile-time snapshot taken at bean construction
    // would be frozen at ~0 for the whole run — but seeding them keeps a caller
    // that reads the fields directly consistent with the getters.
    // GC: `create_string`, then three stores through `obj`.
    let pin = ctx.pin_native_root(*obj);
    // zt t27/lg1 (`CRATONVM_VM_IDENTITY_HOTSPOT`): the VM's
    // `sun.management.compiler`, which HotSpot's `CompilationImpl` reads
    // (`HotSpot 64-Bit Tiered Compilers`), this VM's own name without it.
    let compiler = ctx
        .get_system_property("sun.management.compiler")
        .unwrap_or_else(|| "CratonVM JIT".to_string());
    let name = ctx.create_string(&compiler);
    *obj = ctx.read_native_pin(pin, *obj);
    ctx.unpin_native_roots(pin);
    ctx.set_field(*obj, 0, Value::Object(Some(name)));
    let compile_ms = ctx.jit_total_compile_time_ms();
    ctx.set_field(
        *obj,
        1,
        Value::Long(compile_ms.map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX))),
    );
    ctx.set_field(*obj, 2, Value::Int(i32::from(compile_ms.is_some())));
}

pub(in crate::jmx) fn register_compilation_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/CompilationMXBean";
    // Interface — synthetic receivers only. `getName()` reads slot 0, which
    // is null on an unconstructed bean.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mut this = this;
        init_compilation_mxbean_fields(ctx, &mut this);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // REAL: the JIT's own cumulative compile timer,
    // `jit::tiered::CompilationStats::total_compile_time_ms` (an `AtomicU64`
    // every finished task adds to in `CompilerCore::complete_task`), reached via
    // the escalated `NativeContext::jit_total_compile_time_ms` accessor. It is
    // already in the JMX spec's unit, so no conversion. Read LIVE rather than
    // from slot 1: a bean allocated at boot would otherwise report the ~0 ms
    // that had accumulated by then for the rest of the run. Slot 1 remains the
    // fallback for a VM with no JIT accounting.
    r.register(cls, "getTotalCompilationTime", "()J", |ctx, args| {
        if let Some(ms) = ctx.jit_total_compile_time_ms() {
            return Ok(Some(Value::Long(i64::try_from(ms).unwrap_or(i64::MAX))));
        }
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // REAL, and deliberately driven by the SAME accessor call shape as the
    // getter above: the pair cannot drift into claiming support for a number
    // that is not kept, because "supported" is defined as "the accessor returned
    // Some". `false` survives for a VM whose JIT keeps no accounting, which is
    // what keeps the 0 above legible as "not measured" rather than "measured and
    // zero". Kept in lock-step with the sibling registration in
    // `phases_late::management` (which registers the same triple).
    r.register(
        cls,
        "isCompilationTimeMonitoringSupported",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                ctx.jit_total_compile_time_ms().is_some(),
            ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// 8b. jdk.management.VirtualThreadSchedulerMXBean — 1-field synthetic
// ---------------------------------------------------------------------------
//
// `ProcessInfo.getVirtualThreads()` (Spring Boot's
// `org.springframework.boot.info.ProcessInfoTests.virtualThreadsInfoIfAvailable`)
// probes `ClassUtils.isPresent("jdk.management.VirtualThreadSchedulerMXBean")`
// then `ManagementFactory.getPlatformMXBean(...)`. The interface class itself
// is real JDK (loaded from the real `java.management`/`jdk.management`
// modules, so `isPresent` already succeeds); the gap was
// `getPlatformMXBean` falling through its match to `None` for this class,
// so the whole feature read as "unavailable" on a JRE that HotSpot itself
// would report it present for.
//
// `getParallelism()` uses the same real, already-tracked
// `available_processor_count()` accessor `Runtime.availableProcessors()`
// uses — it's the actual default virtual-thread-scheduler parallelism
// (`ForkJoinPool.commonPool()`-style sizing) unless
// `jdk.virtualThreadScheduler.parallelism` overrides it, which CratonVM
// doesn't track separately. `getPoolSize()` / `getMountedVirtualThreadCount()`
// / `getQueuedVirtualThreadCount()` are REAL as of 2026-07-28 — they read the
// live `ForkJoinScheduler` counters through `NativeContext::vt_scheduler_stats`;
// see the note on their registrations below.
pub(in crate::jmx) fn alloc_virtual_thread_scheduler_mxbean(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj: ObjectRef =
        try_alloc_concurrent_synthetic(ctx, "jdk/management/VirtualThreadSchedulerMXBean", 1)?;
    ctx.set_field(obj, 0, Value::Int(ctx.available_processor_count()));
    Ok(obj)
}

pub(in crate::jmx) fn register_virtual_thread_scheduler_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "jdk/management/VirtualThreadSchedulerMXBean";
    r.register(cls, "getParallelism", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // REAL(round-trip): `getParallelism()` above reads slot 0, so accepting and
    // ignoring the write made the JMM's documented
    // `setParallelism(n)`/`getParallelism()` round-trip dead — a JMX client
    // could set the target parallelism and read back the old value forever.
    // Store it, and reject the sizes the spec rejects rather than silently
    // recording nonsense. (CratonVM still runs no separately-sized carrier pool
    // to physically resize; what is fixed here is the bean's own state, which
    // is the part callers observe.)
    r.register(cls, "setParallelism", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if size <= 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("parallelism must be greater than zero: {size}"),
            }
            .into());
        }
        ctx.set_field(this, 0, Value::Int(size));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // REAL (the three below), via the escalated
    // `NativeContext::vt_scheduler_stats` accessor, which returns
    // `(pool_size, mounted, queued)` from `ForkJoinScheduler::live_carriers()`
    // / `busy_carriers()` / `queued_len()`. The previous 0s were an honest floor
    // (all three are legitimately 0 before any virtual thread runs) but a floor
    // is not a measurement, and the numbers had existed in
    // `vm/src/threading/virtual_threads.rs` all along.
    //
    // ONE accessor call per native, and one accessor returning the whole triple:
    // three separate reads would sample the scheduler at three different
    // instants and could report `mounted > pool_size`. Each getter discards the
    // two counters it does not need rather than re-sampling for them. `None`
    // (no scheduler running) keeps the 0 floor, which is then the true answer.
    r.register(cls, "getPoolSize", "()I", |ctx, _args| {
        let stats = ctx.vt_scheduler_stats();
        Ok(Some(Value::Int(stats.map_or(0, |(pool, _, _)| pool))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getMountedVirtualThreadCount", "()I", |ctx, _args| {
        let stats = ctx.vt_scheduler_stats();
        Ok(Some(Value::Int(stats.map_or(0, |(_, mounted, _)| mounted))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getQueuedVirtualThreadCount", "()J", |ctx, _args| {
        let stats = ctx.vt_scheduler_stats();
        Ok(Some(Value::Long(stats.map_or(0, |(_, _, queued)| queued))))
    }, cratonvm_native_api::NativeKind::Bridge);
    register_bound_virtual_thread_scheduler(r);
    }

/// Live virtual threads: every one is a `ThreadBuilders$BoundVirtualThread`,
/// i.e. it owns a dedicated carrier OS thread for its whole life
/// (`ContinuationSupport.isSupported0` is 0, so the scheduler is the "bound" one).
pub(in crate::jmx) fn live_virtual_thread_count(ctx: &mut dyn NativeContext) -> i32 {
    let mut count = 0i32;
    for thread in ctx.enumerate_threads(usize::MAX) {
        if cratonvm_native_builtins::is_virtual_thread_object(ctx, thread) {
            count += 1;
        }
    }
    count
}

/// `VirtualThreadSchedulerMXBean` of the carrier-bound scheduler, the bean a real
/// JDK image hands out here.
///
/// With no continuation support the JDK picks `BoundVirtualThreadSchedulerImpl`
/// (`VirtualThreadSchedulerImpls.create`), whose counters are all `-1`
/// ("unavailable"). HotSpot has continuation support, so the bean there answers
/// real numbers and `ProcessInfo.getVirtualThreads()` (Spring Boot's
/// `ProcessInfoTests.virtualThreadsInfoIfAvailable`) requires them to be `>= 0`.
/// This VM's scheduler can answer all three truthfully, so it does:
///
/// * mounted: every live virtual thread is mounted, on its own carrier;
/// * pool size: the carriers, one per live virtual thread;
/// * queued: `0`, there is no scheduler queue (each carrier runs its one thread).
///
/// `Intrinsic`: the real bodies are constants (`-1`), so no `Bridge` could rank
/// above them; the parallelism getter keeps the JDK's own answer. `--compatible`
/// registers and dispatches the same rows. The interface-keyed registrations
/// above serve the synthetic bean only.
pub(in crate::jmx) fn register_bound_virtual_thread_scheduler(r: &mut NativeMethodRegistry) {
    const CLASS: &str =
        "com/sun/management/internal/VirtualThreadSchedulerImpls$BoundVirtualThreadSchedulerImpl";
    r.register(
        CLASS,
        "getMountedVirtualThreadCount",
        "()I",
        |ctx, _args| Ok(Some(Value::Int(live_virtual_thread_count(ctx)))),
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        CLASS,
        "getPoolSize",
        "()I",
        |ctx, _args| Ok(Some(Value::Int(live_virtual_thread_count(ctx)))),
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        CLASS,
        "getQueuedVirtualThreadCount",
        "()J",
        |_ctx, _args| Ok(Some(Value::Long(0))),
        cratonvm_native_api::NativeKind::Intrinsic,
    );
}

// ---------------------------------------------------------------------------
// 9. GarbageCollectorMXBean — 4-field synthetic
// ---------------------------------------------------------------------------

pub(in crate::jmx) fn alloc_gc_mxbean(
    ctx: &mut dyn NativeContext,
    descriptor: Option<&cratonvm_native_api::GcCollectorInfo>,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj =
        try_alloc_concurrent_synthetic(ctx, "java/lang/management/GarbageCollectorMXBean", 4)?;
    let mut obj = obj;
    init_gc_mxbean_fields(ctx, &mut obj, descriptor);
    Ok(obj)
}

/// The historical synthetic bean's pool names, kept for a collector that
/// describes no beans (the names match no pool; see
/// `gengc-r4w2-obs-jmx-residuals-on-the-generational-backend` item 3).
pub(in crate::jmx) const LEGACY_SYNTHETIC_GC_POOLS: &[&str] = &["Eden", "Survivor", "Old Gen"];

/// Populate the 4 synthetic `GarbageCollectorMXBean` slots — shared by the
/// factory path and the `<init>` native.
///
/// gen r5w2/obs6 (2026-09-26): from `descriptor` — one of the running
/// collector's own beans (`NativeContext::gc_collectors`) — when it describes
/// them: its name, count, time and pool names, so this bean no longer calls
/// itself `CratonVM GC`, lists `Eden` / `Survivor` / `Old Gen` (names no pool
/// carries) or counts a full cycle twice (`VmHeap::collection_count`, the
/// relocation epoch, bumps both of its halves). `None` keeps the historical
/// answers byte-for-byte (G1 / ZGC without bean state, mocks).
pub(in crate::jmx) fn init_gc_mxbean_fields(
    ctx: &mut dyn NativeContext,
    obj: &mut ObjectRef,
    descriptor: Option<&cratonvm_native_api::GcCollectorInfo>,
) {
    let (name, count, time, pools) = match descriptor {
        Some(d) => (d.name, d.count as i64, d.time_ms as i64, d.pool_names),
        None => (
            "CratonVM GC",
            ctx.gc_collection_count() as i64,
            0,
            LEGACY_SYNTHETIC_GC_POOLS,
        ),
    };
    // GC: every reference below crosses an allocation — `obj`, the pool-name
    // array, and each name string minted before the next `create_string`.
    // A `NativeHandleScope` roots them all.
    let mut scope = NativeHandleScope::new(ctx);
    let obj_h = scope.root(*obj);
    let name = scope.create_string(name);
    let this = scope.get(&obj_h);
    scope.set_field(this, 0, Value::Object(Some(name)));
    scope.set_field(this, 1, Value::Long(count)); // collectionCount (real)
    scope.set_field(this, 2, Value::Long(time)); // collectionTime
                                                 // Field 3 = memoryPoolNames: `String[]`, the declared type of
                                                 // `getMemoryPoolNames()` (i7-L2), which hands this array out. No-load
                                                 // lookup: `String` is always loaded.
    let string_component =
        cratonvm_native_builtins::lang_class::reflection_component_id(&mut *scope, "java/lang/String");
    let pool_names = scope.new_ref_array(string_component, pools.len());
    let arr_h = scope.root(pool_names);
    for (i, pool) in pools.iter().enumerate() {
        let s = scope.create_string(pool);
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(s)));
    }
    let arr = scope.get(&arr_h);
    let this = scope.get(&obj_h);
    scope.set_field(this, 3, Value::Object(Some(arr)));
    *obj = this;
}

/// The collector descriptor a synthetic `GarbageCollectorMXBean` was built
/// from, looked up by the name in its slot 0; `None` for the historical
/// `CratonVM GC` bean and on a collector that describes no beans. gen
/// r5w2/obs6.
pub(in crate::jmx) fn synthetic_gc_bean_descriptor(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<cratonvm_native_api::GcCollectorInfo> {
    let collectors = ctx.gc_collectors();
    if collectors.is_empty() {
        return None;
    }
    let name = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    collectors.into_iter().find(|c| c.name == name)}

pub(in crate::jmx) fn register_gc_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/GarbageCollectorMXBean";
    // Interface — synthetic receivers only. `getName()` / `getMemoryPoolNames()`
    // read slots 0 and 3, both null on an unconstructed bean (a null
    // `String[]` return from `getMemoryPoolNames()` NPEs its callers).
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mut this = this;
        // gen r5w2/obs6: the collector's first bean when it describes them
        // (Serial's `Copy` on Generational), the historical bean otherwise.
        let first = ctx.gc_collectors().into_iter().next();
        init_gc_mxbean_fields(ctx, &mut this, first.as_ref());
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // H2's Utils.collectGarbage() spins until getCollectionTime() increments
    // (src/main/org/h2/util/Utils.java:288-294). If these return frozen
    // synthetic fields, the loop is infinite — observed as a > 1 hour hang
    // on H2 TestAll boot. Bridge to the real heap counter so each GC bumps
    // the value. getCollectionTime is real accumulated pause time on the
    // generational backend (2026-09-23; rounded up per collection so it still
    // moves on every one) and the count stand-in elsewhere — see
    // `NativeContext::gc_collection_time_ms`.
    //
    // gen r5w2/obs6: a bean built from one of the collector's own descriptors
    // (`init_gc_mxbean_fields`) answers that bean's live count and time, as
    // the canonical bean of the same name does — every collection counted
    // once, under the bean it belongs to. The historical `CratonVM GC` bean
    // (no descriptors) keeps the totals, unchanged.
    r.register(cls, "getCollectionCount", "()J", |ctx, args| {
        let described = obj_arg(args, 0)
            .ok()
            .and_then(|this| synthetic_gc_bean_descriptor(&*ctx, this));
        Ok(Some(Value::Long(match described {
            Some(d) => i64::try_from(d.count).unwrap_or(i64::MAX),
            None => ctx.gc_collection_count() as i64,
        })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getCollectionTime", "()J", |ctx, args| {
        let described = obj_arg(args, 0)
            .ok()
            .and_then(|this| synthetic_gc_bean_descriptor(&*ctx, this));
        Ok(Some(Value::Long(match described {
            Some(d) => i64::try_from(d.time_ms).unwrap_or(i64::MAX),
            None => ctx.gc_collection_time_ms() as i64,
        })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cls,
        "getMemoryPoolNames",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP: this synthetic bean has no `isValid` slot to read (the 4 fields
    // are name/count/time/poolNames), and it only ever exists because
    // `alloc_gc_mxbean` just built it — a collector bean handed out by the
    // factory is valid by construction. The concrete `sun.management`
    // manager/pool beans, which DO carry the flag, read it instead (see
    // `native_manager_is_valid`).
    r.register(cls, "isValid", "()Z", |_ctx, _args| Ok(Some(Value::Int(1))), cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// 10. MBeanServer — in-process synthetic MBean registry
//
// `management` ships a working (if minimal) in-process JMX agent. The
// synthetic `javax/management/MBeanServer` instance carries its registry on
// the heap so the moving GC traces it like any other object — no Rust-side
// static handle table (which a moving GC would silently relocate out from
// under us). Field layout:
//
//   slot 0  defaultDomain  : String           ("DefaultDomain")
//   slot 1  mbeanCount      : Integer          (cached count, kept in sync)
//   slot 2  names           : Object[]         (registry keys — ObjectName
//                                               canonical-name strings)
//   slot 3  beans           : Object[]         (registered MBean ObjectRefs,
//                                               index-parallel to `names`)
//
// On `registerMBean` we grow both parallel arrays by one. `getAttribute` /
// `setAttribute` / `invoke` look the bean up by ObjectName key and dispatch
// the JavaBean accessor (`getXxx` / `setXxx`) or the named operation against
// the stored MBean object via `invoke_virtual`. This is the same dispatch
// the JDK's `StandardMBean` performs reflectively, minus the OpenType
// translation layer.
//
// The registry capacity is fixed-grown (a fresh, one-larger array each
// register); MBean registration is rare and one-shot at boot, so the O(n)
// copy is irrelevant and avoids needing a resizable backing structure in a
// synthetic field.
// ---------------------------------------------------------------------------

/// Slot indices on the synthetic MBeanServer.
pub(in crate::jmx) const MBS_DOMAIN: usize = 0;
pub(in crate::jmx) const MBS_COUNT: usize = 1;
pub(in crate::jmx) const MBS_NAMES: usize = 2;
pub(in crate::jmx) const MBS_BEANS: usize = 3;
/// Parallel array of the ORIGINAL `javax.management.ObjectName` objects the
/// caller registered under. `MBS_NAMES` stores their canonical key *strings*
/// (used for identity/dedup, which is canonical per JMX), but a query must
/// return the original ObjectName objects: `ObjectName.toString()` preserves
/// the as-constructed key order (e.g. `Tomcat:type=Valve,name=...`) whereas the
/// canonical form sorts keys (`Tomcat:name=...,type=Valve`). Reconstructing
/// from the canonical string would therefore change `toString()` and break
/// callers that compare the textual form (e.g. Tomcat's `TestRegistration`).
pub(in crate::jmx) const MBS_ONAMES: usize = 4;

/// Number of fields on the synthetic MBeanServer (must cover all slots above).
pub(in crate::jmx) const MBS_NUM_FIELDS: usize = 5;

/// Allocate the in-process platform MBeanServer with an empty registry.
pub(in crate::jmx) fn alloc_mbean_server(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    Ok(alloc_mbean_server_with_domain(ctx, None)?)
}

/// Allocate an in-process MBeanServer whose default domain follows the
/// `MBeanServerFactory` overload that created it.
pub(in crate::jmx) fn alloc_mbean_server_with_domain(
    ctx: &mut dyn NativeContext,
    domain: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/management/MBeanServer", MBS_NUM_FIELDS)?;
    Ok(mbs_init_registry(
        ctx,
        obj,
        domain.unwrap_or("DefaultDomain"),
    ))
}

/// Give the MBeanServer `obj` its default domain and an empty registry.
/// Returns `obj` at its current address.
///
/// gc-common w19-e: both copies of this (`alloc_mbean_server_with_domain` and
/// `MBeanServer.<init>`) stored the domain and the three arrays through the
/// server's address from before four allocations, each array across the next
/// one's allocation too.
pub(in crate::jmx) fn mbs_init_registry(ctx: &mut dyn NativeContext, obj: ObjectRef, domain: &str) -> ObjectRef {
    let mut scope = NativeHandleScope::new(ctx);
    let obj_h = scope.root(obj);
    let domain_obj = scope.create_string(domain);
    let domain_h = scope.root(domain_obj);
    let names = scope.new_ref_array(ClassId::new(0), 0);
    let names_h = scope.root(names);
    let beans = scope.new_ref_array(ClassId::new(0), 0);
    let beans_h = scope.root(beans);
    let onames = scope.new_ref_array(ClassId::new(0), 0);
    let obj = scope.get(&obj_h);
    let domain_obj = scope.get(&domain_h);
    let names = scope.get(&names_h);
    let beans = scope.get(&beans_h);
    scope.set_field(obj, MBS_DOMAIN, Value::Object(Some(domain_obj)));
    scope.set_field(obj, MBS_COUNT, Value::Int(0));
    scope.set_field(obj, MBS_NAMES, Value::Object(Some(names)));
    scope.set_field(obj, MBS_BEANS, Value::Object(Some(beans)));
    scope.set_field(obj, MBS_ONAMES, Value::Object(Some(onames)));
    obj
}

/// Resolve a stable registry key for an `ObjectName` argument. Tries the
/// real-JDK `getCanonicalName()` first, then `toString()`, then a direct
/// `read_string` (synthetic ObjectName stubs sometimes ARE the string).
/// Returns the empty string when the argument is null/unreadable so a
/// caller can still register/look up under a deterministic key rather than
/// panicking.
pub(in crate::jmx) fn object_name_key(ctx: &mut dyn NativeContext, name: Option<ObjectRef>) -> String {
    let name = match name {
        Some(n) => n,
        None => return String::new(),
    };
    // gc-common w19-e: `getCanonicalName()` is Java, and `toString()` and the
    // last-resort read used the name's address from before it. Rooted.
    let mut scope = NativeHandleScope::new(ctx);
    let name_h = scope.root(name);
    for (m, d) in [
        ("getCanonicalName", "()Ljava/lang/String;"),
        ("toString", "()Ljava/lang/String;"),
    ] {
        let name_now = scope.get(&name_h);
        if let Ok(Some(Value::Object(Some(s)))) = scope.invoke_virtual(name_now, m, d, &[]) {
            if let Some(text) = scope.read_string(s) {
                if !text.is_empty() {
                    return canonical_object_name_text(&text);
                }
            }
        }
    }
    // Last resort: the object might itself be a String (synthetic stub).
    let name_now = scope.get(&name_h);
    canonical_object_name_text(&scope.read_string(name_now).unwrap_or_default())
}

/// [`object_name_key`] for an `MBeanServer` native that keeps using its
/// receiver and the name afterwards: both are handed back at their CURRENT
/// addresses.
///
/// gc-common w16-f: `object_name_key` runs `getCanonicalName()` /
/// `toString()` -- Java -- and every registry native called it and then
/// walked the receiver's registry arrays (`mbs_find`, `mbs_registry`, the
/// stores of a registration) and stored or returned the `ObjectName`, all
/// through the addresses from before that call.
pub(in crate::jmx) fn mbs_key(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    name: &mut Option<ObjectRef>,
) -> String {
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(*this);
    let name_h = name.map(|n| scope.root(n));
    let key = object_name_key(&mut *scope, *name);
    *this = scope.get(&this_h);
    *name = name_h.as_ref().map(|h| scope.get(h));
    key
}

/// Read the current registry (names, beans) arrays off a server object.
pub(in crate::jmx) fn mbs_registry(
    ctx: &dyn NativeContext,
    server: ObjectRef,
) -> (Option<ObjectRef>, Option<ObjectRef>) {
    let names = match ctx.get_field(server, MBS_NAMES) {
        Value::Object(opt) => opt,
        _ => None,
    };
    let beans = match ctx.get_field(server, MBS_BEANS) {
        Value::Object(opt) => opt,
        _ => None,
    };
    (names, beans)}

/// Read the parallel array of original `ObjectName` objects off a server.
pub(in crate::jmx) fn mbs_onames(ctx: &dyn NativeContext, server: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(server, MBS_ONAMES) {
        Value::Object(opt) => opt,
        _ => None,
    }
}

/// Find the registry index for `key`, or None.
pub(in crate::jmx) fn mbs_find(ctx: &dyn NativeContext, server: ObjectRef, key: &str) -> Option<usize> {
    let (names, _) = mbs_registry(ctx, server);
    let names = names?;
    let len = ctx.array_length(names);
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(names, i) {
            if ctx.read_string(s).as_deref() == Some(key) {
                return Some(i);
            }
        }
    }
    None
}

/// Capitalise the first ASCII letter of `s` (JavaBean accessor naming).
pub(in crate::jmx) fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Look up the registered MBean ObjectRef for `key`, or None.
pub(in crate::jmx) fn mbs_lookup_bean(ctx: &dyn NativeContext, server: ObjectRef, key: &str) -> Option<ObjectRef> {
    let idx = mbs_find(ctx, server, key)?;
    let (_, beans) = mbs_registry(ctx, server);
    match ctx.get_array_element(beans?, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Build a typed JMX exception so callers' declared `throws` contracts and
/// catch clauses keep working. Falling back to IllegalArgumentException is
/// only for an unrecoverable class-materialisation failure during bootstrap.
pub(in crate::jmx) fn jmx_exception(ctx: &mut dyn NativeContext, class: &str, message: String) -> MethodCallFailed {
    let message_ref = ctx.create_string(&message);
    match ctx.new_object_initialized(
        class,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message_ref))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IllegalArgumentException { message }.into(),
    }
}

/// This VM's platform MBeanServer, created on first use.
///
/// The store's lock is NOT held across the allocation (gc-common w8-f; it
/// used to be). The allocation path runs Java (`ensure_class_initialized` in
/// `try_alloc_concurrent_synthetic` can run a `<clinit>`), and Java can reach
/// a collection -- whose root scan (`gc_scan_platform_mbean_server_root_for_vm`)
/// takes this same lock, on this thread (a `std` mutex re-locked: deadlock) or
/// on another thread waiting for this one to park. Two first callers of one VM
/// may therefore both allocate; the first to publish wins and the other's
/// object is garbage. Nothing between the allocation's return and the publish
/// can collect, so the fresh object's address is still good when stored.
pub(in crate::jmx) fn platform_mbean_server(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if let Some(&server) = platform_mbean_server_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&vm)
    {
        return Ok(server);
    }
    let fresh = alloc_mbean_server(ctx)?;
    let mut store = platform_mbean_server_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    Ok(*store.entry(vm).or_insert(fresh))
}

/// `javax.management.InstanceNotFoundException` for an absent MBean.
pub(in crate::jmx) fn jmx_instance_not_found(ctx: &mut dyn NativeContext, key: &str) -> MethodCallFailed {
    jmx_exception(
        ctx,
        "javax/management/InstanceNotFoundException",
        key.to_string(),
    )
}

/// Shared body of `MBeanServer.add/removeNotificationListener(ObjectName,
/// NotificationListener, NotificationFilter, Object)`; `method` is the name to
/// forward to the target MBean's own `NotificationBroadcaster` overload.
///
/// Resolves the ObjectName against this server's registry, then hands the
/// listener to the bean when the bean can actually broadcast. A bean that is
/// not a broadcaster is accepted without recording anything: nothing in this
/// synthetic registry ever fires a Notification, so there is no delivery to
/// promise and no listener state worth keeping.
///
/// CARVE-OUT: an unregistered name in the `JMImplementation:` domain is
/// accepted rather than reported missing. That domain holds the
/// `MBeanServerDelegate` every real `MBeanServer` owns and this registry does
/// not materialize, and it is exactly what registration-notification listeners
/// (Tomcat's `StatusManagerServlet.init()`) subscribe to — reporting it absent
/// would turn a call that succeeds on every real JVM into a servlet 500.
pub(in crate::jmx) fn mbs_notification_listener(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> MethodCallResult {
    // `javax.management.NotificationBroadcaster` / `NotificationEmitter`.
    const BROADCASTER_DESC: &str = "(Ljavax/management/NotificationListener;Ljavax/management/NotificationFilter;Ljava/lang/Object;)V";
    let mut this = obj_arg(args, 0)?;
    let mut name_ref = match args.get(1) {
        Some(Value::Object(opt)) => *opt,
        _ => None,
    };
    // gc-common w16-f: the listener, filter and handback arguments are rooted
    // across the key's Java calls too; they were forwarded from their
    // pre-call addresses.
    let mut scope = NativeHandleScope::new(ctx);
    let fwd: Vec<Option<NativeHandle>> = (2..5)
        .map(|i| match args.get(i) {
            Some(Value::Object(Some(o))) => Some(scope.root(*o)),
            _ => None,
        })
        .collect();
    let key = mbs_key(&mut *scope, &mut this, &mut name_ref);
    let bean = match mbs_lookup_bean(&*scope, this, &key) {
        Some(bean) => bean,
        None if key.starts_with("JMImplementation:") => return Ok(None),
        None => return Err(jmx_instance_not_found(&mut *scope, &key)),
    };
    let bean_class = scope
        .class_name_of_id(scope.class_id_of_object(bean))
        .unwrap_or_default();
    if scope.method_exists(&bean_class, method, BROADCASTER_DESC) {
        let call_args: Vec<Value> = fwd
            .iter()
            .map(|h| Value::Object(h.as_ref().map(|h| scope.get(h))))
            .collect();
        scope.invoke_virtual(bean, method, BROADCASTER_DESC, &call_args)?;
    }
    Ok(None)
}

/// `javax.management.AttributeNotFoundException` for an absent attribute.
pub(in crate::jmx) fn jmx_attribute_not_found(ctx: &mut dyn NativeContext, attr: &str) -> MethodCallFailed {
    jmx_exception(
        ctx,
        "javax/management/AttributeNotFoundException",
        attr.to_string(),
    )
}

/// Build a `javax.management.ObjectInstance(name, className)` for `bean` under
/// `name`. `className` is the bean's runtime class (dotted), empty if unknown.
pub(in crate::jmx) fn build_object_instance(
    ctx: &mut dyn NativeContext,
    name: Option<ObjectRef>,
    bean: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w16-f: the bean's class is read BEFORE anything allocates
    // (it was read off the bean's address after the ObjectInstance's
    // allocation), the class-name String is built before the ObjectInstance,
    // and the name is rooted across both allocations.
    let cls_name = bean
        .map(|b| {
            ctx.class_name_of_id(ctx.class_id_of_object(b))
                .unwrap_or_default()
        })
        .unwrap_or_default()
        .class_text(&*ctx);
    let mut scope = NativeHandleScope::new(ctx);
    let name_h = name.map(|n| scope.root(n));
    let cls_name_str = scope.create_string(&cls_name);
    let cls_h = scope.root(cls_name_str);
    let oi: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "javax/management/ObjectInstance", 2)?;
    let name = name_h.as_ref().map(|h| scope.get(h));
    let cls_name_str = scope.get(&cls_h);
    scope.set_field_by_name(oi, "name", Value::Object(name));
    scope.set_field_by_name(oi, "className", Value::Object(Some(cls_name_str)));
    Ok(oi)
}

/// Resolve the `ObjectName` at registry index `i`. Prefers the ORIGINAL
/// registered ObjectName object (slot `MBS_ONAMES`, which preserves
/// `toString()` key order); if that wasn't captured, reconstructs one from the
/// canonical key string in `MBS_NAMES`. Returns `(ref, is_object_name)` where
/// `is_object_name` is false only when reconstruction failed and the raw key
/// string is returned as a last resort (so pattern matching is skipped for it).
pub(in crate::jmx) fn mbs_resolve_oname(
    ctx: &mut dyn NativeContext,
    server: ObjectRef,
    i: usize,
) -> Option<(ObjectRef, bool)> {
    if let Some(arr) = mbs_onames(ctx, server) {
        if i < ctx.array_length(arr) {
            if let Value::Object(Some(o)) = ctx.get_array_element(arr, i) {
                return Some((o, true));
            }
        }
    }
    // Original ObjectName not captured — reconstruct from the canonical key.
    let (names_opt, _) = mbs_registry(ctx, server);
    let key_s = match names_opt.map(|a| ctx.get_array_element(a, i)) {
        Some(Value::Object(Some(s))) => s,
        _ => return None,
    };
    let on = ctx
        .invoke(
            "javax/management/ObjectName",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/management/ObjectName;",
            &[Value::Object(Some(key_s))],
        )
        .ok()
        .flatten()
        .and_then(|v| match v {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        });
    match on {
        Some(o) => Some((o, true)),
        None => Some((key_s, false)),
    }
}

/// Core of `queryNames` / `queryMBeans`: build a real `Set` of the registered
/// entries whose ObjectName matches `pattern` (a null pattern matches all). When
/// `as_instances` is true the set holds `ObjectInstance`s (queryMBeans),
/// otherwise the original `ObjectName`s (queryNames). Pattern matching is
/// delegated to the real `ObjectName.apply(ObjectName)` bytecode, so wildcard
/// domains, key-property subset/pattern matching and `:*` all follow JMX
/// semantics exactly. GC-safe via per-iteration pinning of the candidate and a
/// persistent pin of the accumulator set and server.
pub(in crate::jmx) fn mbs_query_set(
    ctx: &mut dyn NativeContext,
    server: ObjectRef,
    pattern: Option<ObjectRef>,
    as_instances: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let server_pin = ctx.pin_native_root(server);
    let pat_pin = pattern.map(|p| ctx.pin_native_root(p));
    let set = match ctx.new_object_initialized("java/util/HashSet", "()V", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => {
            // `new_object_initialized` refused (a unit-test mock with no JDK
            // classes): best-effort UNFILTERED set of the original names,
            // preserving prior behaviour. It is built through
            // `cratonvm_native_builtins::build_real_hash_set`, which allocates and invokes
            // `<init>` itself -- the shape this arm used to fall back to wrote
            // an element array at absolute slot 0 and a count at slot 1, which
            // is the MAP layout on a class whose one real field is `map`, so
            // every real `Set` method answered for an empty set. There is no
            // mode in which that second shape was right, and its raw slot
            // writes were part of what pinned `java/util/HashSet`'s slot floor.
            let s = ctx.read_native_pin(server_pin, server);
            let len = mbs_onames(ctx, s).map(|a| ctx.array_length(a)).unwrap_or(0);
            let mut elems = Vec::with_capacity(len);
            if let Some(arr) = mbs_onames(ctx, s) {
                for i in 0..len {
                    if let Value::Object(Some(o)) = ctx.get_array_element(arr, i) {
                        elems.push(o);
                    }
                }
            }
            ctx.unpin_native_roots(server_pin);
            return Ok(cratonvm_native_builtins::build_real_hash_set(ctx, &elems)?);
        }
    };
    let set_pin = ctx.pin_native_root(set);
    let len = {
        let s = ctx.read_native_pin(server_pin, server);
        mbs_onames(ctx, s)
            .or_else(|| mbs_registry(ctx, s).0)
            .map(|a| ctx.array_length(a))
            .unwrap_or(0)
    };
    for i in 0..len {
        let s = ctx.read_native_pin(server_pin, server);
        let (on_ref, is_on) = match mbs_resolve_oname(ctx, s, i) {
            Some(t) => t,
            None => continue,
        };
        let on_pin = ctx.pin_native_root(on_ref);
        let matched = match (pattern, is_on) {
            (Some(_), true) => {
                let pat = ctx.read_native_pin(pat_pin.unwrap(), pattern.unwrap());
                let cand = ctx.read_native_pin(on_pin, on_ref);
                matches!(
                    ctx.invoke_virtual(
                        pat,
                        "apply",
                        "(Ljavax/management/ObjectName;)Z",
                        &[Value::Object(Some(cand))],
                    ),
                    Ok(Some(Value::Int(x))) if x != 0
                )
            }
            // Null pattern (match all), or a non-ObjectName fallback key.
            _ => true,
        };
        if matched {
            let elem = if as_instances {
                let s2 = ctx.read_native_pin(server_pin, server);
                let bean = mbs_lookup_bean_at(ctx, s2, i);
                let cand = ctx.read_native_pin(on_pin, on_ref);
                build_object_instance(ctx, Some(cand), bean)
            } else {
                Ok(ctx.read_native_pin(on_pin, on_ref))
            }?;
            let elem_pin = ctx.pin_native_root(elem);
            let set_now = ctx.read_native_pin(set_pin, set);
            let e = ctx.read_native_pin(elem_pin, elem);
            let _ = ctx.invoke_virtual(
                set_now,
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(e))],
            );
            ctx.unpin_native_roots(elem_pin);
        }
        ctx.unpin_native_roots(on_pin);
    }
    let result = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(server_pin);
    Ok(result)
}

/// Read the registered bean at registry index `i`, or None.
pub(in crate::jmx) fn mbs_lookup_bean_at(ctx: &dyn NativeContext, server: ObjectRef, i: usize) -> Option<ObjectRef> {
    let (_, beans) = mbs_registry(ctx, server);
    match ctx.get_array_element(beans?, i) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Return the registered JMX domains in their first-registration order.
///
/// `MBeanServer.getDomains()` is defined in terms of the names currently
/// registered with the server, not its configured default domain.  The
/// synthetic registry already keeps canonical ObjectName strings in
/// `MBS_NAMES`; deriving the distinct prefix before `:` from those strings
/// keeps this result in lock-step with registerMBean/unregisterMBean without
/// duplicating mutable state.  A no-domain ObjectName is registered in the
/// server's default domain, matching the JMX registration contract.
pub(in crate::jmx) fn mbs_domains(ctx: &mut dyn NativeContext, server: ObjectRef) -> Vec<String> {
    let default_domain = match ctx.get_field(server, MBS_DOMAIN) {
        Value::Object(Some(domain)) => ctx
            .read_string(domain)
            .unwrap_or_else(|| "DefaultDomain".to_string()),
        _ => "DefaultDomain".to_string(),
    };
    let names = match ctx.get_field(server, MBS_NAMES) {
        Value::Object(Some(names)) => Some(names),
        _ => None,
    };
    let Some(names) = names else {
        return Vec::new();
    };
    let onames = mbs_onames(ctx, server);

    let mut domains: Vec<String> = Vec::new();
    for i in 0..ctx.array_length(names) {
        let Value::Object(Some(name)) = ctx.get_array_element(names, i) else {
            continue;
        };
        let Some(name) = ctx.read_string(name) else {
            continue;
        };
        // Prefer ObjectName.getDomain(): MBS_NAMES is a registry key used for
        // lookup and may be a synthetic fallback representation, while the
        // original ObjectName carries the authoritative JMX domain.
        let object_name_domain = onames
            .filter(|onames| i < ctx.array_length(*onames))
            .and_then(|onames| match ctx.get_array_element(onames, i) {
                Value::Object(Some(oname)) => {
                    match ctx.invoke_virtual(oname, "getDomain", "()Ljava/lang/String;", &[]) {
                        Ok(Some(Value::Object(Some(domain)))) => ctx.read_string(domain),
                        _ => None,
                    }
                }
                _ => None,
            });
        let domain = object_name_domain.unwrap_or_else(|| {
            name.split_once(':')
                .map(|(domain, _)| domain)
                .filter(|domain| !domain.is_empty())
                .unwrap_or(default_domain.as_str())
                .to_string()
        });
        if !domains.iter().any(|existing| existing == &domain) {
            domains.push(domain);
        }
    }
    domains
}

pub fn register_mbean_server(r: &mut NativeMethodRegistry) {
            let cls = "javax/management/MBeanServer";
    r.register(cls, "<init>", "()V", |ctx, args| {
        // Initialise the registry on a freshly-constructed synthetic server
        // so a directly-`new`'d instance (not via getPlatformMBeanServer)
        // also has a usable empty registry.
        let this = obj_arg(args, 0)?;
        mbs_init_registry(ctx, this, "DefaultDomain");
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(
        cls,
        "getDefaultDomain",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match ctx.get_field(this, MBS_DOMAIN) {
                v @ Value::Object(Some(_)) => Ok(Some(v)),
                _ => {
                    let d = ctx.create_string("DefaultDomain");
                    Ok(Some(Value::Object(Some(d))))
                }
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getMBeanCount",
        "()Ljava/lang/Integer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Compute the live count from the registry array so the answer
            // can never drift from the actual number of registered beans.
            let (names, _) = mbs_registry(ctx, this);
            let count = names.map(|n| ctx.array_length(n)).unwrap_or(0);
            // Box as java.lang.Integer (getMBeanCount returns Integer).
            let boxed = ctx.invoke(
                "java/lang/Integer",
                "valueOf",
                "(I)Ljava/lang/Integer;",
                &[Value::Int(count as i32)],
            );
            match boxed {
                Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
                // Fall back to the cached primitive slot if Integer.valueOf
                // isn't available in this context (e.g. unit-test mock).
                _ => Ok(Some(Value::Int(count as i32))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // getDomains() -> the distinct domains represented by registered
    // ObjectNames.  This must be native on the synthetic interface receiver:
    // otherwise invokeinterface falls through to MBeanServer's abstract
    // declaration and fails with AbstractMethodError (no Code attribute).
    r.register(cls, "getDomains", "()[Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let domains = mbs_domains(ctx, this);
        // `String[]` (i7-L2), rooted across each `create_string` (the array
        // used to be held raw across them).
        let result = cratonvm_native_builtins::t27_tls::materialize_java_string_array(ctx, &domains);
        Ok(Some(Value::Object(Some(result))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // add/removeNotificationListener(ObjectName, NotificationListener,
    // NotificationFilter, Object).
    //
    // These used to accept unconditionally and DROP the registration, so every
    // listener a JMX client installed through the server was silently connected
    // to nothing — including listeners on MBeans that DO broadcast. Two things
    // in the real contract are recoverable here, and `mbs_notification_listener`
    // implements both: the target MBean has to exist (these methods are declared
    // `throws InstanceNotFoundException`, which is the answer for an
    // unregistered ObjectName), and when the target is itself a
    // NotificationBroadcaster the listener belongs ON THE BEAN, where its own
    // broadcaster support delivers notifications — the same delegation the real
    // `DefaultMBeanServerInterceptor` performs.
    r.register(
        cls,
        "addNotificationListener",
        "(Ljavax/management/ObjectName;Ljavax/management/NotificationListener;Ljavax/management/NotificationFilter;Ljava/lang/Object;)V",
        |ctx, args| mbs_notification_listener(ctx, args, "addNotificationListener"), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "removeNotificationListener",
        "(Ljavax/management/ObjectName;Ljavax/management/NotificationListener;Ljavax/management/NotificationFilter;Ljava/lang/Object;)V",
        |ctx, args| mbs_notification_listener(ctx, args, "removeNotificationListener"), cratonvm_native_api::NativeKind::Bridge,
    );

    // registerMBean(Object, ObjectName) -> ObjectInstance. We store the
    // bean under its ObjectName key and return the ObjectName-bearing
    // ObjectInstance (callers mostly ignore the return or read getObjectName).
    r.register(
        cls,
        "registerMBean",
        "(Ljava/lang/Object;Ljavax/management/ObjectName;)Ljavax/management/ObjectInstance;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let bean = match args.get(1) {
                Some(Value::Object(Some(b))) => *b,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "registerMBean: null MBean instance".to_string(),
                    }
                    .into())
                }
            };
            let name_ref = match args.get(2) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            // gc-common w16-f: ONE scope roots the receiver, the bean and the
            // name for the whole registration. The key's `getCanonicalName()`
            // is Java and the growth path makes four allocations (three
            // arrays and the key String); the receiver, the bean, the name,
            // the three source arrays and the new arrays allocated first were
            // all used through their addresses from before them. The source
            // arrays are re-read off the receiver after the allocations.
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let bean_h = scope.root(bean);
            let name_h = name_ref.map(|n| scope.root(n));
            let key = object_name_key(&mut *scope, name_ref);
            // Grow the parallel registry arrays by one (or overwrite an
            // existing entry with the same key — last registration wins,
            // matching a re-register after unregister). `MBS_ONAMES` holds the
            // original ObjectName objects so queries return them verbatim.
            let this = scope.get(&this_h);
            let (names_opt, _) = mbs_registry(&*scope, this);
            let old_len = names_opt.map(|n| scope.array_length(n)).unwrap_or(0);
            if let Some(idx) = mbs_find(&*scope, this, &key) {
                // Overwrite in place.
                let (_, beans_opt) = mbs_registry(&*scope, this);
                let onames_opt = mbs_onames(&*scope, this);
                let bean = scope.get(&bean_h);
                let name_ref = name_h.as_ref().map(|h| scope.get(h));
                if let Some(beans) = beans_opt {
                    scope.set_array_element(beans, idx, Value::Object(Some(bean)));
                }
                if let Some(onames) = onames_opt {
                    scope.set_array_element(onames, idx, Value::Object(name_ref));
                }
            } else {
                let new_names = scope.new_ref_array(ClassId::new(0), old_len + 1);
                let new_names_h = scope.root(new_names);
                let new_beans = scope.new_ref_array(ClassId::new(0), old_len + 1);
                let new_beans_h = scope.root(new_beans);
                let new_onames = scope.new_ref_array(ClassId::new(0), old_len + 1);
                let new_onames_h = scope.root(new_onames);
                let key_str = scope.create_string(&key);
                // Every address from here on is current: nothing below
                // allocates.
                let this = scope.get(&this_h);
                let (names_opt, beans_opt) = mbs_registry(&*scope, this);
                let onames_opt = mbs_onames(&*scope, this);
                let new_names = scope.get(&new_names_h);
                let new_beans = scope.get(&new_beans_h);
                let new_onames = scope.get(&new_onames_h);
                let bean = scope.get(&bean_h);
                let name_ref = name_h.as_ref().map(|h| scope.get(h));
                for i in 0..old_len {
                    if let Some(names) = names_opt {
                        let v = scope.get_array_element(names, i);
                        scope.set_array_element(new_names, i, v);
                    }
                    if let Some(beans) = beans_opt {
                        let v = scope.get_array_element(beans, i);
                        scope.set_array_element(new_beans, i, v);
                    }
                    if let Some(onames) = onames_opt {
                        let v = scope.get_array_element(onames, i);
                        scope.set_array_element(new_onames, i, v);
                    }
                }
                scope.set_array_element(new_names, old_len, Value::Object(Some(key_str)));
                scope.set_array_element(new_beans, old_len, Value::Object(Some(bean)));
                scope.set_array_element(new_onames, old_len, Value::Object(name_ref));
                scope.set_field(this, MBS_NAMES, Value::Object(Some(new_names)));
                scope.set_field(this, MBS_BEANS, Value::Object(Some(new_beans)));
                scope.set_field(this, MBS_ONAMES, Value::Object(Some(new_onames)));
                scope.set_field(this, MBS_COUNT, Value::Int((old_len + 1) as i32));
            }

            // Build an ObjectInstance(name, className) for the return value.
            let bean = scope.get(&bean_h);
            let name_ref = name_h.as_ref().map(|h| scope.get(h));
            Ok(Some(Value::Object(Some(build_object_instance(
                &mut *scope,
                name_ref,
                Some(bean),
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // unregisterMBean(ObjectName) — remove the entry if present.
    r.register(
        cls,
        "unregisterMBean",
        "(Ljavax/management/ObjectName;)V",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let mut name_ref = match args.get(1) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            // `mbs_key`: the receiver's current address (gc-common w16-f).
            let key = mbs_key(ctx, &mut this, &mut name_ref);
            if let Some(idx) = mbs_find(ctx, this, &key) {
                let (names_opt, beans_opt) = mbs_registry(ctx, this);
                let onames_opt = mbs_onames(ctx, this);
                let old_len = names_opt.map(|n| ctx.array_length(n)).unwrap_or(0);
                if old_len > 0 {
                    // Three allocations in a row: each one can move the arrays
                    // allocated before it, and the receiver and the three
                    // source arrays as well. Root everything, then read every
                    // address back once the last allocation is behind us.
                    let mut scope = NativeHandleScope::new(ctx);
                    let this_h = scope.root(this);
                    let names_src_h = names_opt.map(|n| scope.root(n));
                    let beans_src_h = beans_opt.map(|b| scope.root(b));
                    let onames_src_h = onames_opt.map(|o| scope.root(o));
                    let new_names_obj = scope.new_ref_array(ClassId::new(0), old_len - 1);
                    let new_names_h = scope.root(new_names_obj);
                    let new_beans_obj = scope.new_ref_array(ClassId::new(0), old_len - 1);
                    let new_beans_h = scope.root(new_beans_obj);
                    let new_onames = scope.new_ref_array(ClassId::new(0), old_len - 1);
                    let new_names = scope.get(&new_names_h);
                    let new_beans = scope.get(&new_beans_h);
                    let names_opt = names_src_h.as_ref().map(|h| scope.get(h));
                    let beans_opt = beans_src_h.as_ref().map(|h| scope.get(h));
                    let onames_opt = onames_src_h.as_ref().map(|h| scope.get(h));
                    let this = scope.get(&this_h);
                    let mut w = 0usize;
                    for rd in 0..old_len {
                        if rd == idx {
                            continue;
                        }
                        if let Some(names) = names_opt {
                            let v = scope.get_array_element(names, rd);
                            scope.set_array_element(new_names, w, v);
                        }
                        if let Some(beans) = beans_opt {
                            let v = scope.get_array_element(beans, rd);
                            scope.set_array_element(new_beans, w, v);
                        }
                        if let Some(onames) = onames_opt {
                            let v = scope.get_array_element(onames, rd);
                            scope.set_array_element(new_onames, w, v);
                        }
                        w += 1;
                    }
                    scope.set_field(this, MBS_NAMES, Value::Object(Some(new_names)));
                    scope.set_field(this, MBS_BEANS, Value::Object(Some(new_beans)));
                    scope.set_field(this, MBS_ONAMES, Value::Object(Some(new_onames)));
                    scope.set_field(this, MBS_COUNT, Value::Int((old_len - 1) as i32));                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        cls,
        "isRegistered",
        "(Ljavax/management/ObjectName;)Z",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let mut name_ref = match args.get(1) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            let key = mbs_key(ctx, &mut this, &mut name_ref);
            Ok(Some(Value::Int(mbs_find(ctx, this, &key).is_some() as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getObjectInstance(ObjectName) -> ObjectInstance for a registered bean.
    r.register(
        cls,
        "getObjectInstance",
        "(Ljavax/management/ObjectName;)Ljavax/management/ObjectInstance;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let mut name_ref = match args.get(1) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            let key = mbs_key(ctx, &mut this, &mut name_ref);
            let idx = match mbs_find(ctx, this, &key) {
                Some(i) => i,
                None => return Err(jmx_instance_not_found(ctx, &key)),
            };
            let bean = mbs_lookup_bean_at(ctx, this, idx);
            Ok(Some(Value::Object(Some(build_object_instance(
                ctx, name_ref, bean,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // queryNames(ObjectName, QueryExp) -> Set<ObjectName>. The ObjectName
    // pattern is honoured via the real `ObjectName.apply` (domain + key-property
    // pattern / wildcards / `:*`); a null pattern matches all. QueryExp (the
    // second arg) is not evaluated — JMX clients pass null here for plain
    // name-pattern enumeration, which is the boot/management usage. The set is a
    // real `java.util.HashSet` of the ORIGINAL registered ObjectNames.
    r.register(
        cls,
        "queryNames",
        "(Ljavax/management/ObjectName;Ljavax/management/QueryExp;)Ljava/util/Set;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pattern = match args.get(1) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            Ok(Some(Value::Object(Some(mbs_query_set(
                ctx, this, pattern, false,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // queryMBeans(ObjectName, QueryExp) -> Set<ObjectInstance>. Same pattern
    // matching as queryNames; the set holds ObjectInstances built from the
    // original ObjectName + the registered bean's class.
    r.register(
        cls,
        "queryMBeans",
        "(Ljavax/management/ObjectName;Ljavax/management/QueryExp;)Ljava/util/Set;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pattern = match args.get(1) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            Ok(Some(Value::Object(Some(mbs_query_set(
                ctx, this, pattern, true,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // setAttribute(ObjectName, Attribute) — dispatch setXxx on the bean.
    r.register(
        cls,
        "setAttribute",
        "(Ljavax/management/ObjectName;Ljavax/management/Attribute;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name_ref = match args.get(1) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            let attr = match args.get(2) {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "setAttribute: null Attribute".to_string(),
                    }
                    .into())
                }
            };
            // gc-common w16-f: the receiver, the Attribute and then the bean
            // are rooted across the three Java calls before the setter (the
            // key's `getCanonicalName()`, `getName()`, `getValue()`). Each was
            // used through its address from before them; the bean -- the
            // setter's receiver -- across all three.
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let attr_h = scope.root(attr);
            let key = object_name_key(&mut *scope, name_ref);
            let this = scope.get(&this_h);
            let bean = match mbs_lookup_bean(&*scope, this, &key) {
                Some(b) => b,
                None => return Err(jmx_instance_not_found(&mut *scope, &key)),
            };
            let bean_h = scope.root(bean);
            // Attribute.getName() / getValue().
            let attr = scope.get(&attr_h);
            let attr_name = match scope.invoke_virtual(attr, "getName", "()Ljava/lang/String;", &[])
            {
                Ok(Some(Value::Object(Some(s)))) => scope.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let attr = scope.get(&attr_h);
            let attr_val = scope
                .invoke_virtual(attr, "getValue", "()Ljava/lang/Object;", &[])
                .ok()
                .flatten()
                .unwrap_or(Value::Object(None));
            let setter = format!("set{}", capitalize(&attr_name));
            let bean = scope.get(&bean_h);
            // Try the common Object-typed setter signature first.
            let _ = scope.invoke_virtual(bean, &setter, "(Ljava/lang/Object;)V", &[attr_val]);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // invoke(ObjectName, String op, Object[] params, String[] sig) -> Object.
    r.register(
        cls,
        "invoke",
        "(Ljavax/management/ObjectName;Ljava/lang/String;[Ljava/lang/Object;[Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name_ref = match args.get(1) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            let op_name_ref = match args.get(2) {
                Some(Value::Object(Some(s))) => Some(*s),
                _ => None,
            };
            let op_name = op_name_ref
                .and_then(|s| ctx.read_string(s))
                .unwrap_or_default();
            let params = match args.get(3) {
                Some(Value::Object(Some(a))) => Some(*a),
                _ => None,
            };
            let signature = match args.get(4) {
                Some(Value::Object(Some(a))) => Some(*a),
                _ => None,
            };
            // gc-common w16-f: everything forwarded is rooted across the
            // key's Java call and the first `invoke`; the operation name, the
            // params and the signature were passed on -- and the params read
            // for the fallback -- from their addresses before both, and the
            // bean was the fallback's receiver after the first.
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let op_h = op_name_ref.map(|o| scope.root(o));
            let params_h = params.map(|p| scope.root(p));
            let sig_h = signature.map(|s| scope.root(s));
            let key = object_name_key(&mut *scope, name_ref);
            let this = scope.get(&this_h);
            let bean = match mbs_lookup_bean(&*scope, this, &key) {
                Some(b) => b,
                None => return Err(jmx_instance_not_found(&mut *scope, &key)),
            };
            let bean_h = scope.root(bean);
            let op_name_ref = op_h.as_ref().map(|h| scope.get(h));
            let params = params_h.as_ref().map(|h| scope.get(h));
            let signature = sig_h.as_ref().map(|h| scope.get(h));
            // DynamicMBean (e.g. Tomcat modeler's BaseModelMBean, which wraps
            // a real managed resource like HostConfig) exposes operations
            // through its own invoke(String, Object[], String[]) -- that
            // reflects into the WRAPPED RESOURCE's real method, not a method
            // literally named op_name on the registered bean/wrapper
            // itself. Try that first, passing the real params/signature
            // arrays through faithfully (mirrors the getAttribute delegation
            // above, which already does this for attribute reads).
            if let Ok(Some(v)) = scope.invoke_virtual(
                bean,
                "invoke",
                "(Ljava/lang/String;[Ljava/lang/Object;[Ljava/lang/String;)Ljava/lang/Object;",
                &[
                    Value::Object(op_name_ref),
                    Value::Object(params),
                    Value::Object(signature),
                ],
            ) {
                return Ok(Some(v));
            }
            // That was Java: the bean and the params at their current
            // addresses (gc-common w16-f).
            let bean = scope.get(&bean_h);
            let params = params_h.as_ref().map(|h| scope.get(h));
            let ctx = &mut *scope;
            // Fall back: build a descriptor of (Ljava/lang/Object;)* matching
            // the arity, which dispatches to a no-arg or N-Object-arg method
            // directly on bean. Covers plain user MBeans (a raw registered
            // object whose operation IS a real Object-typed method on it),
            // which is what this path was originally written for.
            let mut call_args: Vec<Value> = Vec::new();
            let mut desc = String::from("(");
            if let Some(arr) = params {
                let n = ctx.array_length(arr);
                for i in 0..n {
                    call_args.push(ctx.get_array_element(arr, i));
                    desc.push_str("Ljava/lang/Object;");
                }
            }
            desc.push_str(")Ljava/lang/Object;");
            match ctx.invoke_virtual(bean, &op_name, &desc, &call_args)? {
                Some(v) => Ok(Some(v)),
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getMBeanInfo(ObjectName) -> MBeanInfo. We don't synthesise a full
    // descriptor model; return null when the bean is registered (callers
    // that need the structured info take the JDK StandardMBean path) and
    // raise InstanceNotFound when it isn't, which is the JMX-correct error.
    r.register(
        cls,
        "getMBeanInfo",
        "(Ljavax/management/ObjectName;)Ljavax/management/MBeanInfo;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let mut name_ref = match args.get(1) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            let key = mbs_key(ctx, &mut this, &mut name_ref);
            if mbs_find(ctx, this, &key).is_none() {
                return Err(jmx_instance_not_found(ctx, &key));
            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        cls,
        "getAttribute",
        "(Ljavax/management/ObjectName;Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            // First try the in-process registry: if the ObjectName resolves
            // to a bean we registered, dispatch the JavaBean accessor
            // (`getXxx` / `isXxx`) against it. This makes a full
            // register -> getAttribute round-trip work for user MBeans.
            let this = obj_arg(args, 0)?;
            let name_ref = match args.get(1) {
                Some(Value::Object(opt)) => *opt,
                _ => None,
            };
            let attr_name = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            // gc-common w16-f: the receiver and the attribute-name String are
            // rooted across the key's Java call, and the bean across every
            // accessor attempt. The name String was forwarded, and the bean
            // re-dispatched on after each failed attempt, through addresses
            // from before those Java calls.
            let bean_found = {
                let mut scope = NativeHandleScope::new(ctx);
                let this_h = scope.root(this);
                let name_s_h = match args.get(2) {
                    Some(Value::Object(Some(s))) => Some(scope.root(*s)),
                    _ => None,
                };
                let key = object_name_key(&mut *scope, name_ref);
                let this = scope.get(&this_h);
                match mbs_lookup_bean(&*scope, this, &key) {
                    None => false,
                    Some(bean) => {
                        let bean_h = scope.root(bean);
                        // DynamicMBean (e.g. Tomcat's modeler BaseModelMBean / a
                        // RequiredModelMBean) exposes attributes through its own
                        // `getAttribute(String)` rather than JavaBean accessors —
                        // delegate to it first so descriptor-driven attributes resolve.
                        if let Some(name_s_h) = name_s_h.as_ref() {
                            let name_s = scope.get(name_s_h);
                            if let Ok(Some(v)) = scope.invoke_virtual(
                                bean,
                                "getAttribute",
                                "(Ljava/lang/String;)Ljava/lang/Object;",
                                &[Value::Object(Some(name_s))],
                            ) {
                                return Ok(Some(v));
                            }
                        }
                        let cap = capitalize(&attr_name);
                        // Try getXxx()Object, then getXxx()-with-real-return via the
                        // generic Object return, then isXxx()Z for boolean attrs.
                        for (m, d) in [
                            (format!("get{cap}"), "()Ljava/lang/Object;".to_string()),
                            (format!("is{cap}"), "()Z".to_string()),
                            (format!("is{cap}"), "()Ljava/lang/Boolean;".to_string()),
                        ] {
                            let bean = scope.get(&bean_h);
                            if let Ok(Some(v)) = scope.invoke_virtual(bean, &m, &d, &[]) {
                                return Ok(Some(v));
                            }
                        }
                        true
                    }
                }
            };
            if bean_found {
                // Registered bean but no accessor matched — JMX says
                // AttributeNotFound.
                return Err(jmx_attribute_not_found(ctx, &attr_name));
            }

            // Fall through: the platform `java.lang:type=*` MXBean
            // attributes that WildFly / boot code queries directly without
            // registering anything in our in-process registry (e.g.
            //   server.getAttribute(ObjectName("java.lang:type=OperatingSystem"),
            //                       "MaxFileDescriptorCount")
            // then `.toString()` + `Long.parseLong`).
            //
            // We answer each attribute with REAL VM state where a source
            // exists, and with the OpenJDK "unavailable" sentinel (-1 / -1.0)
            // — NOT a fabricated plausible number — where it does not. A
            // fabricated plausible value is exactly the invented-but-plausible
            // value the no-synthetic-stubs policy forbids: a consumer could
            // not tell it apart from a real reading.
            //
            // Sources used:
            //   AvailableProcessors -> available_parallelism (real)
            //   Name / Arch / Version
            //                       -> the os.name / os.arch / os.version
            //                          system properties (real), the same
            //                          source `init_os_mxbean_fields` reads.
            //                          These attributes ARE those properties;
            //                          answering from `std::env::consts`
            //                          instead made this server row disagree
            //                          with the direct bean read in one VM.
            //   *PhysicalMemory* / *Swap* / *FileDescriptor* /
            //   CommittedVirtualMemorySize / ProcessCpuTime
            //                       -> -1  (no in-VM source; spec sentinel)
            //   Process/SystemCpuLoad / SystemLoadAverage
            //                       -> -1.0 (spec sentinel for "unavailable")
            // For any unrecognised attribute we return null (the JMX-correct
            // "no such attribute" answer) rather than a fabricated string;
            // callers' existing AttributeNotFound / Throwable handlers cope.
            let response: Option<String> = match attr_name.as_str() {
                "AvailableProcessors" => {
                    // Container-aware (cgroup CPU quota under container support).
                    Some(ctx.available_processor_count().to_string())
                }
                "Name" => Some(
                    ctx.get_system_property("os.name")
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| std::env::consts::OS.to_string()),
                ),
                "Arch" => Some(
                    ctx.get_system_property("os.arch")
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| std::env::consts::ARCH.to_string()),
                ),
                "Version" => Some(
                    ctx.get_system_property("os.version")
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "unknown".to_string()),
                ),
                // No real in-VM source — honest "unavailable" sentinel (-1),
                // which still parses as a Long for callers like WildFly's
                // fd-limit check (which treats a negative value as "skip").
                "MaxFileDescriptorCount"
                | "OpenFileDescriptorCount"
                | "TotalPhysicalMemorySize"
                | "FreePhysicalMemorySize"
                | "TotalSwapSpaceSize"
                | "FreeSwapSpaceSize"
                | "CommittedVirtualMemorySize"
                | "ProcessCpuTime" => Some("-1".to_string()),
                // No CPU-load measurement — spec sentinel for "unavailable".
                "ProcessCpuLoad" | "SystemCpuLoad" | "SystemLoadAverage" => {
                    Some("-1.0".to_string())
                }
                // Unknown attribute: return null (JMX "no such attribute").
                _ => None,
            };
            match response {
                Some(s) => Ok(Some(Value::Object(Some(ctx.create_string(&s))))),
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    }

/// Synthetic-Real-JDK `MBeanServerFactory.createMBeanServer`/`newMBeanServer`
/// overrides, producing our in-process server (built on the *interface*
/// `javax/management/MBeanServer`).
///
/// **Must NOT be called from the real-JDK registration path.** In real-JDK
/// mode, `ManagementFactory.getPlatformMBeanServer()` bytecode calls
/// `MBeanServerFactory.createMBeanServer()`, whose real JDK implementation
/// constructs a concrete `com.sun.jmx.mbeanserver.JmxMBeanServer` — a class
/// that declares `registerMBean`/`addNotificationListener`/etc. with a Code
/// attribute, so interface dispatch resolves correctly (the KAFKA-MBEAN note
/// in `register_management_factory` explains why `getPlatformMBeanServer`
/// itself is deliberately left unregistered to let that real bytecode run).
/// If this override is *also* registered in real-JDK mode, it intercepts
/// `createMBeanServer` before the real bytecode ever constructs
/// `JmxMBeanServer`, handing back our synthetic interface-typed object
/// instead — which throws `AbstractMethodError: ... has no Code attribute`
/// on every un-overridden `MBeanServer` method (e.g.
/// `addNotificationListener`), exactly the failure the KAFKA-MBEAN fix was
/// meant to avoid. Under `management` synthetic-JDK mode there is no
/// real `java.management` module, so the factory call needs a server —
/// returning our synthetic `alloc_mbean_server` gives the full
/// register/get/set/invoke/query flow a concrete receiver there.
///
/// `newMBeanServer` only builds a server; `createMBeanServer` additionally
/// registers it so the (un-overridden, real-bytecode) `findMBeanServer`
/// reports it — matching the JMX contract where only createMBeanServer
/// tracks servers in the factory list.
pub fn register_mbean_server_factory_synthetic(r: &mut NativeMethodRegistry) {
            let new_server: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = |ctx, args| {
        let domain = args.first().and_then(|value| match value {
            Value::Object(Some(value)) => ctx.read_string(*value),
            _ => None,
        });
        Ok(Some(Value::Object(Some(alloc_mbean_server_with_domain(
            ctx,
            domain.as_deref(),
        )?))))
    };
    let create_server: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = |ctx, args| {
        let domain = args.first().and_then(|value| match value {
            Value::Object(Some(value)) => ctx.read_string(*value),
            _ => None,
        });
        let server = alloc_mbean_server_with_domain(ctx, domain.as_deref())?;
        Ok(Some(Value::Object(Some(track_created_mbean_server(
            ctx, server,
        )))))
    };
    for (name, desc) in [
        ("createMBeanServer", "()Ljavax/management/MBeanServer;"),
        (
            "createMBeanServer",
            "(Ljava/lang/String;)Ljavax/management/MBeanServer;",
        ),
    ] {
        r.register(
            "javax/management/MBeanServerFactory",
            name,
            desc,
            create_server, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    for (name, desc) in [
        ("newMBeanServer", "()Ljavax/management/MBeanServer;"),
        (
            "newMBeanServer",
            "(Ljava/lang/String;)Ljavax/management/MBeanServer;",
        ),
    ] {
        r.register(
            "javax/management/MBeanServerFactory",
            name,
            desc,
            new_server, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    }

/// Append a freshly-created MBeanServer to the real
/// `javax.management.MBeanServerFactory.mBeanServerList` static field, so the
/// (un-overridden) real `findMBeanServer(null)` bytecode reports it — the real
/// `createMBeanServer` does this via the private `addMBeanServer`, which our
/// override bypasses. Returns the (possibly GC-forwarded) server. Best-effort:
/// if the field can't be resolved (e.g. unit-test mock) the server is returned
/// untracked, leaving `findMBeanServer` empty as before.
pub(in crate::jmx) fn track_created_mbean_server(ctx: &mut dyn NativeContext, server: ObjectRef) -> ObjectRef {
    let pin = ctx.pin_native_root(server);
    if let Ok(cls_id) = ctx.ensure_class_initialized("javax/management/MBeanServerFactory") {
        if let Some(idx) = ctx.static_field_index_by_name(cls_id, "mBeanServerList") {
            if let Value::Object(Some(list)) = ctx.get_static_field(cls_id, idx) {
                let s = ctx.read_native_pin(pin, server);
                let _ = ctx.invoke_virtual(
                    list,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(s))],
                );
            }
        }
    }
    let result = ctx.read_native_pin(pin, server);
    ctx.unpin_native_roots(pin);
    result
}

